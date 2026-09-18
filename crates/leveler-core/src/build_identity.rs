//! Which build is actually running.
//!
//! A version number is not an identity. Two binaries can both say
//! `0.2.0-beta.1` and contain different code, and that is not a corner case:
//! replacing the file on disk leaves an already-running daemon executing its
//! original image, so a client can connect to a runtime that reports the same
//! version while behaving like last week's build. That happened, and it cost a
//! day of chasing a "regression" that had already been fixed.
//!
//! So identity carries the revision it was built from, and everything that
//! reports or compares identity uses this one type.

use serde::{Deserialize, Serialize};

/// The build a binary was produced from.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct BuildIdentity {
    /// The package version, e.g. `0.2.0-beta.1`.
    pub version: String,
    /// The source revision. `unknown` when the build had no git available —
    /// which is itself a mismatch against anything, by design.
    pub revision: String,
    /// Built from a modified working tree. Metadata about provenance, not an
    /// identity: use `fingerprint` to tell two builds apart.
    #[serde(default)]
    pub dirty: bool,
    /// Deterministic digest of the exact source this artifact was built from
    /// (`revision` + the working-tree delta). Two runs of the SAME executable
    /// share it, so a dirty development build recognises itself instead of
    /// being retired on every launch; a rebuild from different source does not
    /// share it, so a true new build still replaces the old runtime.
    ///
    /// Empty for a runtime built before this field existed: those fall back to
    /// the conservative `dirty` rule.
    #[serde(default)]
    pub fingerprint: String,
}

impl BuildIdentity {
    /// The identity of the binary making this call, stamped at compile time.
    pub fn current() -> Self {
        Self {
            version: env!("CARGO_PKG_VERSION").to_string(),
            revision: env!("LEVELER_BUILD_COMMIT").to_string(),
            dirty: env!("LEVELER_BUILD_DIRTY") == "true",
            fingerprint: env!("LEVELER_BUILD_FINGERPRINT").to_string(),
        }
    }

    /// Whether the identity was reported at all. A runtime built before
    /// identity existed reports nothing, and nothing is not a match.
    pub fn is_known(&self) -> bool {
        self.known_fingerprint().is_some()
            || (!self.revision.is_empty() && self.revision != "unknown")
    }

    fn known_fingerprint(&self) -> Option<&str> {
        (!self.fingerprint.is_empty() && self.fingerprint != "unknown")
            .then_some(self.fingerprint.as_str())
    }

    /// Whether two builds are the same code.
    ///
    /// The artifact fingerprint is the identity when both sides carry one. It
    /// is what makes a dirty build reusable: running the same file twice is
    /// the same generation, however many uncommitted edits it was built from.
    /// A build with no fingerprint (older daemon) falls back to the legacy
    /// rule — version and revision must match and neither may be dirty.
    pub fn matches(&self, other: &Self) -> bool {
        if !self.is_known() || !other.is_known() {
            return false;
        }
        match (self.known_fingerprint(), other.known_fingerprint()) {
            (Some(mine), Some(theirs)) => mine == theirs,
            _ => {
                !self.dirty
                    && !other.dirty
                    && self.version == other.version
                    && self.revision == other.revision
            }
        }
    }

    /// Short form for diagnostics: `0.2.0-beta.1 (abc123def456-dirty)`.
    pub fn short(&self) -> String {
        let rev = self.revision.get(..12).unwrap_or(&self.revision);
        if self.dirty {
            format!("{} ({rev}-dirty)", self.version)
        } else {
            format!("{} ({rev})", self.version)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn id(version: &str, revision: &str, dirty: bool) -> BuildIdentity {
        BuildIdentity {
            version: version.into(),
            revision: revision.into(),
            dirty,
            fingerprint: String::new(),
        }
    }

    fn fp(version: &str, revision: &str, dirty: bool, fingerprint: &str) -> BuildIdentity {
        BuildIdentity {
            version: version.into(),
            revision: revision.into(),
            dirty,
            fingerprint: fingerprint.into(),
        }
    }

    #[test]
    fn the_same_build_matches_itself() {
        let a = id("0.2.0-beta.1", "abc123", false);
        assert!(a.matches(&a.clone()));
    }

    /// The point of the fingerprint: running the SAME executable twice is one
    /// generation, even when it was built from uncommitted work.
    #[test]
    fn a_dirty_build_matches_its_own_artifact() {
        let a = fp("0.2.0-beta.1", "abc123", true, "abc123-deadbeef");
        assert!(a.matches(&a.clone()));
        // …and a second process running that same file reports the same
        // fingerprint, so it recognizes the first as Current.
        let same_artifact = fp("0.2.0-beta.1", "abc123", true, "abc123-deadbeef");
        assert!(a.matches(&same_artifact));
    }

    /// A rebuild from different source is a different artifact and must still
    /// replace the runtime — the fingerprint, not the `dirty` bit, decides.
    #[test]
    fn a_rebuilt_artifact_does_not_match() {
        let old = fp("0.2.0-beta.1", "abc123", true, "abc123-deadbeef");
        let rebuilt = fp("0.2.0-beta.1", "abc123", true, "abc123-0badcafe");
        assert!(!old.matches(&rebuilt));
        assert!(!rebuilt.matches(&old));
    }

    /// A clean build and a dirty build of the same commit are different
    /// artifacts.
    #[test]
    fn clean_and_dirty_artifacts_do_not_match() {
        let clean = fp("0.2.0-beta.1", "abc123", false, "abc123-0000");
        let dirty = fp("0.2.0-beta.1", "abc123", true, "abc123-deadbeef");
        assert!(!clean.matches(&dirty));
        assert!(!dirty.matches(&clean));
    }

    /// The same fingerprint wins even if the two sides disagree on `dirty`
    /// metadata (e.g. one was stamped before an unrelated edit).
    #[test]
    fn the_fingerprint_is_authoritative_over_dirty_metadata() {
        let a = fp("0.2.0-beta.1", "abc123", true, "abc123-deadbeef");
        let b = fp("0.2.0-beta.1", "abc123", false, "abc123-deadbeef");
        assert!(a.matches(&b));
    }

    /// THE incident: same version, different code. Version-only matching is
    /// what let a stale daemon pass for current.
    #[test]
    fn same_version_different_revision_is_a_mismatch() {
        let client = id("0.2.0-beta.1", "new111", false);
        let runtime = id("0.2.0-beta.1", "old999", false);
        assert!(!client.matches(&runtime));
    }

    /// A build with no fingerprint (an older daemon) keeps the conservative
    /// legacy rule: a dirty tree matches nothing.
    #[test]
    fn legacy_dirty_builds_never_match() {
        let clean = id("0.2.0-beta.1", "abc123", false);
        let dirty = id("0.2.0-beta.1", "abc123", true);
        assert!(!clean.matches(&dirty));
        assert!(!dirty.matches(&clean));
        assert!(!dirty.matches(&dirty.clone()));
    }

    /// A runtime built before identity existed reports nothing. Nothing must
    /// never read as "same" — the caller has to treat it as unknown.
    #[test]
    fn an_unreported_identity_is_never_a_match() {
        let known = id("0.2.0-beta.1", "abc123", false);
        assert!(!known.matches(&BuildIdentity::default()));
        assert!(!BuildIdentity::default().matches(&known));
        assert!(!BuildIdentity::default().is_known());
        assert!(!id("0.2.0-beta.1", "unknown", false).is_known());
    }

    #[test]
    fn the_running_binary_knows_its_own_identity() {
        let me = BuildIdentity::current();
        assert_eq!(me.version, env!("CARGO_PKG_VERSION"));
        assert!(!me.revision.is_empty());
        assert!(
            !me.fingerprint.is_empty(),
            "the build stamps a fingerprint, even when git is unavailable (`unknown`)"
        );
    }

    #[test]
    fn short_form_carries_the_revision_and_dirt() {
        assert_eq!(
            id("0.2.0-beta.1", "abc123def4567890", false).short(),
            "0.2.0-beta.1 (abc123def456)"
        );
        assert!(
            id("0.2.0-beta.1", "abc123def4567890", true)
                .short()
                .ends_with("-dirty)")
        );
    }
}
