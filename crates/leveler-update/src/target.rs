//! Host target and the exact release asset that target expects.
//!
//! There is exactly one asset name for a `(version, host)` pair. The release
//! workflow, the checksum sibling, and the updater all derive it from
//! [`release_asset_name`], so an Apple Silicon machine can never be offered an
//! x86_64 archive or a Windows machine a Unix tarball.

use crate::version::Version;

/// Host target triple used to match GitHub release assets.
///
/// `None` means this build's host is not a published target; the caller must
/// fail closed rather than guess a nearby asset.
pub fn host_target_triple() -> Option<&'static str> {
    match (std::env::consts::OS, std::env::consts::ARCH) {
        ("macos", "aarch64") => Some("aarch64-apple-darwin"),
        ("macos", "x86_64") => Some("x86_64-apple-darwin"),
        ("linux", "x86_64") => Some("x86_64-unknown-linux-gnu"),
        ("linux", "aarch64") => Some("aarch64-unknown-linux-gnu"),
        ("windows", "x86_64") => Some("x86_64-pc-windows-msvc"),
        ("windows", "aarch64") => Some("aarch64-pc-windows-msvc"),
        _ => None,
    }
}

/// The executable name inside the archive for a target triple.
pub fn binary_name(triple: &str) -> &'static str {
    if triple.contains("windows") {
        "leveler.exe"
    } else {
        "leveler"
    }
}

/// File extension of the archive for a target triple (Windows ships `.zip`).
pub fn archive_extension(triple: &str) -> &'static str {
    if triple.contains("windows") {
        "zip"
    } else {
        "tar.gz"
    }
}

/// Expected asset file name for a release (without path), e.g.
/// `leveler-v1.0.0-aarch64-apple-darwin.tar.gz`.
///
/// The release workflow builds exactly this name. Anything else is "no asset
/// for this host", never a fuzzy match.
pub fn release_asset_name(version: &Version, triple: &str) -> String {
    let ext = archive_extension(triple);
    format!("leveler-v{version}-{triple}.{ext}")
}

/// The checksum sibling asset name (`<asset>.sha256`), the frozen contract
/// every release built by the release workflow ships.
pub fn checksum_asset_name(asset: &str) -> String {
    format!("{asset}.sha256")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::version::parse_version;

    fn v(s: &str) -> Version {
        parse_version(s).unwrap()
    }

    #[test]
    fn each_shipped_target_maps_to_exactly_one_asset() {
        for (triple, ext) in [
            ("aarch64-apple-darwin", "tar.gz"),
            ("x86_64-apple-darwin", "tar.gz"),
            ("x86_64-unknown-linux-gnu", "tar.gz"),
            ("aarch64-unknown-linux-gnu", "tar.gz"),
            ("x86_64-pc-windows-msvc", "zip"),
        ] {
            let name = release_asset_name(&v("1.0.0"), triple);
            assert_eq!(name, format!("leveler-v1.0.0-{triple}.{ext}"));
            assert_eq!(checksum_asset_name(&name), format!("{name}.sha256"));
        }
    }

    #[test]
    fn the_windows_archive_carries_the_exe() {
        assert_eq!(binary_name("x86_64-pc-windows-msvc"), "leveler.exe");
        assert_eq!(archive_extension("x86_64-pc-windows-msvc"), "zip");
        assert_eq!(binary_name("aarch64-apple-darwin"), "leveler");
        assert_eq!(archive_extension("aarch64-apple-darwin"), "tar.gz");
    }

    #[test]
    fn a_prerelease_tag_keeps_its_identifiers_in_the_asset_name() {
        assert_eq!(
            release_asset_name(&v("1.0.0-beta.2"), "aarch64-apple-darwin"),
            "leveler-v1.0.0-beta.2-aarch64-apple-darwin.tar.gz"
        );
    }
}
