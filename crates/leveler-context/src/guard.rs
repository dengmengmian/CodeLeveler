//! The stale-write guard: detect that a file changed between the read that
//! shaped the model's patch and the write that applies it.
//!
//! There used to be a repeated-read guard here too, which annotated a read
//! when the model asked for the same unchanged range again. That is a judgement
//! about the model's reasoning, not a fact about the filesystem, and the
//! harness does not make it (`docs/ARCHITECTURE.md` §1.1).

use std::collections::HashMap;
use std::sync::Mutex;

/// Incremental fingerprint for streamed file reads.
#[derive(Debug, Clone, Copy)]
pub struct ContentFingerprint(u64);

impl Default for ContentFingerprint {
    fn default() -> Self {
        Self(0xcbf29ce484222325)
    }
}

impl ContentFingerprint {
    /// Add the next byte chunk.
    pub fn update(&mut self, bytes: &[u8]) {
        for byte in bytes {
            self.0 ^= u64::from(*byte);
            self.0 = self.0.wrapping_mul(0x100000001b3);
        }
    }

    /// Final stable 64-bit value.
    pub fn finish(self) -> u64 {
        self.0
    }
}

/// Fingerprints of files as the agent last saw them, so a write can tell whether
/// the file changed underneath it.
///
/// A model builds a patch from the contents it read. If a `run_command`, another
/// sub-agent, or the user rewrites that file in between, applying the patch would
/// silently discard their change. Untracked files are never blocked: this only
/// catches the read → outside-write → patch sequence.
///
/// The fingerprint is a non-cryptographic hash. It detects accidental drift, not
/// a deliberate collision.
#[derive(Default)]
pub struct FileStateTracker {
    seen: Mutex<HashMap<String, u64>>,
}

impl FileStateTracker {
    /// Fingerprint an in-memory byte slice using the same incremental algorithm
    /// as [`ContentFingerprint`].
    pub fn fingerprint(content: &[u8]) -> u64 {
        let mut fingerprint = ContentFingerprint::default();
        fingerprint.update(content);
        fingerprint.finish()
    }

    /// Record the contents of `path` as the agent now knows them. Call after a
    /// read, and after a write, so the agent's own edits do not look stale.
    pub fn record(&self, path: &str, content: &[u8]) {
        self.record_fingerprint(path, Self::fingerprint(content));
    }

    /// Record a fingerprint produced while streaming the file.
    pub fn record_fingerprint(&self, path: &str, fingerprint: u64) {
        self.seen
            .lock()
            .unwrap()
            .insert(path.to_string(), fingerprint);
    }

    /// Drop any fingerprint for `path`. Call when the file is deleted, so a file
    /// later recreated at the same path is not judged against the dead one.
    pub fn forget(&self, path: &str) {
        self.seen.lock().unwrap().remove(path);
    }

    /// Whether `path` changed since it was last recorded. An untracked path is
    /// never stale — the agent never claimed to know its contents.
    pub fn is_stale(&self, path: &str, current: &[u8]) -> bool {
        match self.seen.lock().unwrap().get(path) {
            Some(&known) => known != Self::fingerprint(current),
            None => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn untracked_file_is_never_stale() {
        let t = FileStateTracker::default();
        assert!(!t.is_stale("never/read.rs", b"anything"));
    }

    #[test]
    fn recorded_file_is_stale_only_when_content_differs() {
        let t = FileStateTracker::default();
        t.record("a.rs", b"one");
        assert!(!t.is_stale("a.rs", b"one"));
        assert!(t.is_stale("a.rs", b"two"));
    }

    #[test]
    fn re_recording_clears_staleness() {
        let t = FileStateTracker::default();
        t.record("a.rs", b"one");
        t.record("a.rs", b"two");
        assert!(!t.is_stale("a.rs", b"two"));
    }

    #[test]
    fn precomputed_fingerprints_match_byte_based_tracking() {
        let bytes = b"streamed content\n";
        let fingerprint = FileStateTracker::fingerprint(bytes);
        let tracker = FileStateTracker::default();
        tracker.record_fingerprint("a.rs", fingerprint);
        assert!(!tracker.is_stale("a.rs", bytes));
    }

    #[test]
    fn forgetting_makes_a_path_untracked_again() {
        let t = FileStateTracker::default();
        t.record("a.rs", b"one");
        t.forget("a.rs");
        assert!(!t.is_stale("a.rs", b"totally different"));
    }
}
