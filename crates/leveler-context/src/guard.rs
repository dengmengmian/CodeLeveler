//! Repeated-read guard (spec §28 RepeatedReadHook): detect when a model reads
//! the same file range over and over so the tool can nudge it to use what it has.
//!
//! Plus the stale-write guard: detect that a file changed between the read that
//! shaped the model's patch and the write that applies it.

use std::collections::HashMap;
use std::hash::{DefaultHasher, Hash, Hasher};
use std::sync::Mutex;

/// Tracks repeated reads of the same (file, range, content version).
pub struct RepeatedReadGuard {
    reads: Mutex<HashMap<String, (u64, u32)>>,
    threshold: u32,
}

impl Default for RepeatedReadGuard {
    fn default() -> Self {
        Self::new(3)
    }
}

impl RepeatedReadGuard {
    /// Create a guard that trips after `threshold` repeats of the same range.
    pub fn new(threshold: u32) -> Self {
        Self {
            reads: Mutex::new(HashMap::new()),
            threshold: threshold.max(1),
        }
    }

    /// Record a read of `key` (e.g. "path:start-end") for the current file
    /// contents. A changed file starts a fresh count so recovery reads after a
    /// command, user edit, or sub-agent write are never mistaken for looping.
    pub fn record(&self, key: &str, content: &[u8]) -> u32 {
        let fingerprint = FileStateTracker::fingerprint(content);
        let mut reads = self.reads.lock().unwrap();
        let entry = reads.entry(key.to_string()).or_insert((fingerprint, 0));
        if entry.0 != fingerprint {
            *entry = (fingerprint, 0);
        }
        entry.1 += 1;
        entry.1
    }

    /// Whether this is a wasteful repeat of an unchanged range. This is a nudge
    /// signal only: `read_file` still returns the requested content so a failed
    /// edit can always recover.
    pub fn tripped(&self, key: &str, content: &[u8]) -> bool {
        self.record(key, content) > self.threshold
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
    fn fingerprint(content: &[u8]) -> u64 {
        let mut hasher = DefaultHasher::new();
        content.hash(&mut hasher);
        hasher.finish()
    }

    /// Record the contents of `path` as the agent now knows them. Call after a
    /// read, and after a write, so the agent's own edits do not look stale.
    pub fn record(&self, path: &str, content: &[u8]) {
        self.seen
            .lock()
            .unwrap()
            .insert(path.to_string(), Self::fingerprint(content));
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
    fn trips_after_threshold() {
        let g = RepeatedReadGuard::new(2);
        assert!(!g.tripped("a:1-10", b"same")); // 1st
        assert!(!g.tripped("a:1-10", b"same")); // 2nd
        assert!(g.tripped("a:1-10", b"same")); // 3rd — over threshold
    }

    #[test]
    fn distinct_ranges_are_independent() {
        let g = RepeatedReadGuard::new(1);
        assert!(!g.tripped("a:1-10", b"same"));
        assert!(!g.tripped("b:1-10", b"same"));
        assert!(g.tripped("a:1-10", b"same"));
    }

    #[test]
    fn changed_content_resets_the_repeat_count() {
        let g = RepeatedReadGuard::new(1);
        assert!(!g.tripped("a:1-10", b"before"));
        assert!(g.tripped("a:1-10", b"before"));
        assert!(!g.tripped("a:1-10", b"after"));
    }

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
    fn forgetting_makes_a_path_untracked_again() {
        let t = FileStateTracker::default();
        t.record("a.rs", b"one");
        t.forget("a.rs");
        assert!(!t.is_stale("a.rs", b"totally different"));
    }
}
