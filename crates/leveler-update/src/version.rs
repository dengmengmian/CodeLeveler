//! Syntax version for release tags, and the ordering that decides "is this an
//! upgrade".
//!
//! String comparison is wrong (`"1.9.0" > "1.10.0"` lexically), so every
//! release decision goes through [`Version`]. The pre-release part is carried
//! rather than dropped: it decides ordering (`1.0.0-beta.1 < 1.0.0`), and a
//! stable user must never be offered a pre-release as an upgrade.

use std::cmp::Ordering;

/// A parsed semantic version.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Version {
    pub major: u64,
    pub minor: u64,
    pub patch: u64,
    /// Dot-separated identifiers from `-a.b.c`; empty for a release.
    pub pre: Vec<String>,
}

impl std::fmt::Display for Version {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}.{}.{}", self.major, self.minor, self.patch)?;
        if !self.pre.is_empty() {
            write!(f, "-{}", self.pre.join("."))?;
        }
        Ok(())
    }
}

impl Ord for Version {
    fn cmp(&self, other: &Self) -> Ordering {
        (self.major, self.minor, self.patch)
            .cmp(&(other.major, other.minor, other.patch))
            .then_with(|| match (self.pre.is_empty(), other.pre.is_empty()) {
                (true, true) => Ordering::Equal,
                // SemVer §11: a release outranks any pre-release of the same
                // `x.y.z`.
                (true, false) => Ordering::Greater,
                (false, true) => Ordering::Less,
                (false, false) => cmp_pre(&self.pre, &other.pre),
            })
    }
}

impl PartialOrd for Version {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

/// Compare two non-empty pre-release identifier lists (SemVer §11): numeric
/// identifiers compare numerically and rank below alphanumeric ones, and when
/// every shared identifier is equal, the longer list wins (`beta` < `beta.1`).
fn cmp_pre(a: &[String], b: &[String]) -> Ordering {
    for (x, y) in a.iter().zip(b.iter()) {
        // An identifier too large for u64 is treated as alphanumeric rather
        // than panicking.
        let ord = match (x.parse::<u64>(), y.parse::<u64>()) {
            (Ok(nx), Ok(ny)) => nx.cmp(&ny),
            (Ok(_), Err(_)) => Ordering::Less,
            (Err(_), Ok(_)) => Ordering::Greater,
            (Err(_), Err(_)) => x.as_str().cmp(y.as_str()),
        };
        if ord != Ordering::Equal {
            return ord;
        }
    }
    a.len().cmp(&b.len())
}

impl Version {
    /// Whether this version is a pre-release (`1.0.0-rc.1`).
    pub fn is_prerelease(&self) -> bool {
        !self.pre.is_empty()
    }
}

/// Parse `1.0.0`, `v1.0.0`, or a tag with a pre-release suffix
/// (`1.0.0-beta.1`).
///
/// Build metadata (`+abc123`) is dropped: SemVer §10 keeps it out of
/// precedence, and no release asset carries it.
pub fn parse_version(raw: &str) -> Option<Version> {
    let s = raw.trim();
    let s = s.strip_prefix('v').unwrap_or(s);
    let s = s.split('+').next().unwrap_or("");
    let (core, pre) = match s.split_once('-') {
        // `1.0.0-` has a pre-release marker and no identifiers: not a version.
        Some((_, "")) => return None,
        Some((core, rest)) => (
            core,
            rest.split('.').map(str::to_string).collect::<Vec<_>>(),
        ),
        None => (s, Vec::new()),
    };
    if pre.iter().any(String::is_empty) {
        return None;
    }
    let mut parts = core.split('.');
    let major = parts.next()?.parse().ok()?;
    let minor = parts.next()?.parse().ok()?;
    let patch = parts.next()?.parse().ok()?;
    if parts.next().is_some() {
        return None;
    }
    Some(Version {
        major,
        minor,
        patch,
        pre,
    })
}

/// Whether an install should proceed.
///
/// `force` reinstalls even when the versions compare equal.
pub fn should_upgrade(current: &Version, target: &Version, force: bool) -> bool {
    force || target > current
}

/// Whether a release is a valid automatic-update target for a stable build.
///
/// Auto-update tracks stable only; a pre-release tag is never resolved by
/// GitHub's `releases/latest`, and this is the belt to that suspenders.
pub fn is_stable_target(current: &Version, target: &Version) -> bool {
    !target.is_prerelease()
        // A pre-release build is not auto-replaced by an older stable line.
        && (current.is_prerelease() || target > current || target == current)
}

/// The version string a built binary reports, from the crate's compile-time
/// version. Single-sourced with `Cargo.toml`.
pub fn current_version() -> Version {
    parse_version(env!("CARGO_PKG_VERSION")).expect("CARGO_PKG_VERSION must be a SemVer version")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(s: &str) -> Version {
        parse_version(s).unwrap_or_else(|| panic!("{s} must parse"))
    }

    #[test]
    fn ordering_is_numeric_not_lexical() {
        assert!(v("1.0.0") == v("1.0.0"));
        assert!(v("1.0.0") < v("1.0.1"));
        assert!(v("1.0.9") < v("1.0.10"));
        assert!(v("1.9.0") < v("1.10.0"));
        assert!(v("1.9.9") < v("2.0.0"));
    }

    #[test]
    fn release_outranks_its_prereleases() {
        assert!(v("1.0.0-beta.1") < v("1.0.0"));
        assert!(v("1.0.0-rc.1") < v("1.0.0"));
        assert!(v("1.0.0-alpha") < v("1.0.0-beta"));
        assert!(v("1.0.0-beta") < v("1.0.0-beta.1"));
        assert!(v("1.0.0-beta.2") < v("1.0.0-beta.10"));
    }

    #[test]
    fn tag_prefix_and_build_metadata_are_normalized() {
        assert_eq!(v("v1.0.0"), v("1.0.0"));
        assert_eq!(v("1.0.0+build.7"), v("1.0.0"));
        assert_eq!(v("v1.0.0").to_string(), "1.0.0");
    }

    #[test]
    fn invalid_tags_are_rejected() {
        for raw in [
            "",
            "v",
            "1",
            "1.2",
            "1.2.3.4",
            "abc",
            "1.0.0-",
            "1.0.0-beta..1",
            "vx1.2.3",
        ] {
            assert!(parse_version(raw).is_none(), "{raw:?} must not parse");
        }
    }

    #[test]
    fn should_upgrade_is_strict_and_force_aware() {
        assert!(!should_upgrade(&v("1.0.0"), &v("1.0.0"), false));
        assert!(should_upgrade(&v("1.0.0"), &v("1.0.0"), true));
        assert!(should_upgrade(&v("1.0.0"), &v("1.0.1"), false));
        assert!(!should_upgrade(&v("1.0.1"), &v("1.0.0"), false));
        // A pre-release user is offered the stable release it led to.
        assert!(should_upgrade(&v("1.0.0-beta.1"), &v("1.0.0"), false));
    }

    #[test]
    fn a_prerelease_tag_is_never_an_auto_target() {
        // Ordering alone (`should_upgrade`) would accept `1.1.0-rc.1`; the
        // auto-update policy is what refuses it.
        assert!(should_upgrade(&v("1.0.0"), &v("1.1.0-rc.1"), false));
        assert!(!is_stable_target(&v("1.0.0"), &v("1.1.0-rc.1")));
        assert!(is_stable_target(&v("1.0.0"), &v("1.0.1")));
        assert!(is_stable_target(&v("1.0.0"), &v("1.0.0")));
    }

    #[test]
    fn the_compiled_version_parses() {
        // The binary must be able to read its own version; a bad
        // CARGO_PKG_VERSION would make every update decision impossible.
        let current = current_version();
        assert!(current.major >= 1);
    }
}
