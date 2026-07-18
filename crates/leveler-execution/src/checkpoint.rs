//! A lightweight checkpoint: the original content of every file a session
//! touches, captured just before the first write (spec §28 CheckpointHook).
//!
//! Restoring rewrites originals and removes files that did not previously exist,
//! so an interrupted or bad run can be rolled back exactly to its starting tree.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

/// Records the pre-modification state of touched files.
#[derive(Default)]
pub struct Checkpoint {
    /// Path -> original bytes (`None` means the file did not exist).
    originals: Mutex<HashMap<PathBuf, Option<Vec<u8>>>>,
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
        let original = std::fs::read(path).ok();
        map.insert(path.to_path_buf(), original);
    }

    /// Clear all captured state, making the current tree the new restore point
    /// (an explicit `create_checkpoint`).
    pub fn reset(&self) {
        self.originals.lock().unwrap().clear();
    }

    /// The number of distinct files captured.
    pub fn touched_count(&self) -> usize {
        self.originals.lock().unwrap().len()
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
                Some(bytes) => {
                    if let Some(parent) = path.parent() {
                        std::fs::create_dir_all(parent)?;
                    }
                    std::fs::write(path, bytes)?;
                }
                None => match std::fs::remove_file(path) {
                    Ok(()) => {}
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                    Err(e) => return Err(e),
                },
            }
        }
        Ok(())
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
