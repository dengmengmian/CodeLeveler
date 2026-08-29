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
    /// Built from a modified working tree. Two dirty builds of the same
    /// revision can differ arbitrarily, so this participates in matching.
    #[serde(default)]
    pub dirty: bool,
}

impl BuildIdentity {
    /// The identity of the binary making this call, stamped at compile time.
    pub fn current() -> Self {
        Self {
            version: env!("CARGO_PKG_VERSION").to_string(),
            revision: env!("LEVELER_BUILD_COMMIT").to_string(),
            dirty: env!("LEVELER_BUILD_DIRTY") == "true",
        }
    }

    /// Whether the identity was reported at all. A runtime built before
    /// identity existed reports nothing, and nothing is not a match.
    pub fn is_known(&self) -> bool {
        !self.revision.is_empty() && self.revision != "unknown"
    }

    /// Whether two builds are the same code.
    ///
    /// Version alone is deliberately not enough. Neither is revision alone
    /// when either side is dirty: a modified tree at revision X is not the
    /// same binary as a clean X, and two dirty trees at X are not each other.
    pub fn matches(&self, other: &Self) -> bool {
        self.is_known()
            && other.is_known()
            && !self.dirty
            && !other.dirty
            && self.version == other.version
            && self.revision == other.revision
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
        }
    }

    #[test]
    fn the_same_build_matches_itself() {
        let a = id("0.2.0-beta.1", "abc123", false);
        assert!(a.matches(&a.clone()));
    }

    /// THE incident: same version, different code. Version-only matching is
    /// what let a stale daemon pass for current.
    #[test]
    fn same_version_different_revision_is_a_mismatch() {
        let client = id("0.2.0-beta.1", "new111", false);
        let runtime = id("0.2.0-beta.1", "old999", false);
        assert!(!client.matches(&runtime));
    }

    /// A dirty tree is not identified by its revision: the working copy can
    /// hold anything. Two dirty builds of the same commit are not the same
    /// binary, and a dirty one never matches a clean one.
    #[test]
    fn dirty_builds_never_match() {
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
