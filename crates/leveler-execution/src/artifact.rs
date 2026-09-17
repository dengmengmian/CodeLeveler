//! Content-addressed storage for large text artifacts (command output that
//! would otherwise be silently truncated). Mirrors the media store's
//! deduplicating pattern: identical content hashes to the same file, so a
//! preview shown to the model can always point at the full, retrievable output.

use std::path::PathBuf;

use sha2::{Digest, Sha256};

/// A stored artifact: where the full content lives, its hash, and its size.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArtifactRef {
    pub path: PathBuf,
    pub sha256: String,
    pub size_bytes: usize,
}

/// A content-addressed text-artifact store rooted at a directory (kept OUTSIDE
/// the workspace so artifacts never pollute the repo or verification scope).
#[derive(Debug, Clone)]
pub struct ArtifactStore {
    root: PathBuf,
}

impl ArtifactStore {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    /// Write `content` to a content-addressed file (`<sha256>.txt`). Identical
    /// content deduplicates to the same path and is not rewritten.
    ///
    /// Secrets are redacted before hashing/writing so artifacts never store
    /// raw API keys or Authorization headers.
    pub fn write_text(&self, content: &str) -> std::io::Result<ArtifactRef> {
        let content = leveler_core::redact_secrets(content);
        let sha256 = hex(&Sha256::digest(content.as_bytes()));
        std::fs::create_dir_all(&self.root)?;
        let path = self.root.join(format!("{sha256}.txt"));
        if !path.exists() {
            std::fs::write(&path, &content)?;
        }
        Ok(ArtifactRef {
            path,
            sha256,
            size_bytes: content.len(),
        })
    }
}

fn hex(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push_str(&format!("{b:02x}"));
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp() -> PathBuf {
        // A per-process atomic counter — NOT a timestamp. Windows' coarse timer
        // resolution (~15ms) let parallel tests collide on the same nanos → same
        // dir, and one test's remove_dir_all raced another's write (Windows
        // forbids deleting a dir with live handles). The counter is unique per
        // call regardless of clock resolution; process id disambiguates across
        // processes.
        use std::sync::atomic::{AtomicU64, Ordering};
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        std::env::temp_dir().join(format!(
            "leveler-artifact-test-{}-{}",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::Relaxed)
        ))
    }

    #[test]
    fn writes_full_content_and_reports_hash_and_size() {
        let root = tmp();
        let store = ArtifactStore::new(&root);
        let content = "a".repeat(100_000);
        let art = store.write_text(&content).unwrap();

        assert_eq!(art.size_bytes, 100_000);
        assert!(art.path.starts_with(&root));
        assert_eq!(std::fs::read_to_string(&art.path).unwrap(), content);
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn redacts_secrets_before_writing() {
        let root = tmp();
        let store = ArtifactStore::new(&root);
        let art = store
            .write_text("Authorization: Bearer sk-abcdefghijklmnop1234")
            .unwrap();
        let stored = std::fs::read_to_string(&art.path).unwrap();
        assert!(!stored.contains("sk-abcdefghijklmnop1234"), "{stored}");
        assert!(stored.contains("[REDACTED]"), "{stored}");
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn identical_content_deduplicates_to_the_same_path() {
        let root = tmp();
        let store = ArtifactStore::new(&root);
        let a = store.write_text("same output").unwrap();
        let b = store.write_text("same output").unwrap();
        assert_eq!(a.path, b.path, "identical content must dedup");
        assert_eq!(a.sha256, b.sha256);

        let c = store.write_text("different").unwrap();
        assert_ne!(c.path, a.path);
        std::fs::remove_dir_all(&root).ok();
    }
}
