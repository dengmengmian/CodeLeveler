//! A lightweight checkpoint: the original content of every file a session
//! touches, captured just before the first write (spec §28 CheckpointHook).
//!
//! Restoring rewrites originals and removes files that did not previously exist,
//! so an interrupted or bad run can be rolled back exactly to its starting tree.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

/// Originals larger than this spill to a temp file instead of staying
/// resident: a session that touches a build artifact or data file must not pin
/// megabytes of RAM per file for the whole run.
const MAX_IN_MEMORY_ORIGINAL: usize = 4 * 1024 * 1024;

/// The captured original state of one touched path.
enum Original {
    /// The file did not exist before the first write.
    Absent,
    InMemory(Vec<u8>),
    /// Large original spilled to a session temp file (removed on reset/drop).
    Spilled(PathBuf),
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
    pub fn record(&self, path: &Path) {
        let mut map = self.originals.lock().unwrap();
        if map.contains_key(path) {
            return;
        }
        let original = match std::fs::read(path).ok() {
            None => Original::Absent,
            Some(bytes) if bytes.len() <= MAX_IN_MEMORY_ORIGINAL => Original::InMemory(bytes),
            Some(bytes) => match spill(&bytes) {
                Ok(spill_path) => Original::Spilled(spill_path),
                // Rollback safety beats memory: keep it resident rather than
                // silently losing the ability to restore.
                Err(e) => {
                    tracing::warn!(
                        path = %path.display(),
                        error = %e,
                        "could not spill large checkpoint original; keeping in memory"
                    );
                    Original::InMemory(bytes)
                }
            },
        };
        map.insert(path.to_path_buf(), original);
    }

    /// Clear all captured state, making the current tree the new restore point
    /// (an explicit `create_checkpoint`).
    pub fn reset(&self) {
        let mut map = self.originals.lock().unwrap();
        remove_spill_files(&map);
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
                Original::InMemory(bytes) => bytes.len(),
                Original::Absent | Original::Spilled(_) => 0,
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
                Original::InMemory(bytes) => {
                    if let Some(parent) = path.parent() {
                        std::fs::create_dir_all(parent)?;
                    }
                    std::fs::write(path, bytes)?;
                }
                Original::Spilled(spill_path) => {
                    if let Some(parent) = path.parent() {
                        std::fs::create_dir_all(parent)?;
                    }
                    std::fs::copy(spill_path, path)?;
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

impl Drop for Checkpoint {
    fn drop(&mut self) {
        if let Ok(map) = self.originals.lock() {
            remove_spill_files(&map);
        }
    }
}

/// Write a large original into a unique session temp file.
fn spill(bytes: &[u8]) -> std::io::Result<PathBuf> {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let path = std::env::temp_dir().join(format!(
        "leveler-checkpoint-{}-{}.orig",
        std::process::id(),
        COUNTER.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::write(&path, bytes)?;
    Ok(path)
}

fn remove_spill_files(map: &HashMap<PathBuf, Original>) {
    for original in map.values() {
        if let Original::Spilled(spill_path) = original {
            std::fs::remove_file(spill_path).ok();
        }
    }
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
        cp.record(&file);
        std::fs::write(&file, "modified").unwrap();
        cp.restore().unwrap();
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "original");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn removes_newly_created_file_on_restore() {
        let dir = tmp();
        let file = dir.join("new.txt");
        let cp = Checkpoint::new();
        cp.record(&file); // does not exist yet
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
        cp.record(&file);
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
    fn first_record_wins() {
        let dir = tmp();
        let file = dir.join("a.txt");
        std::fs::write(&file, "v1").unwrap();
        let cp = Checkpoint::new();
        cp.record(&file);
        std::fs::write(&file, "v2").unwrap();
        cp.record(&file); // ignored
        cp.restore().unwrap();
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "v1");
        std::fs::remove_dir_all(&dir).ok();
    }
}
