//! Host target and the exact release assets that target expects.
//!
//! The updater has exactly one preferred asset name for a `(version, host)`
//! pair. The release workflow, the checksum sibling, and the updater all derive
//! it from [`release_asset_name`], so an Apple Silicon machine can never be
//! offered an x86_64 archive or a Windows machine a Unix tarball. Releases also
//! carry the pre-xz `.tar.gz` sibling: binaries built before the format change
//! select it through [`release_asset_names`], and new binaries fall back to it
//! only for releases that predate xz.

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
///
/// Unix archives are `.tar.xz`: about 40% smaller than the `.tar.gz` assets
/// older releases published. The release workflow still publishes both, so a
/// binary built before this format change can keep updating itself through the
/// gzip asset it knows.
pub fn archive_extension(triple: &str) -> &'static str {
    if triple.contains("windows") {
        "zip"
    } else {
        "tar.xz"
    }
}

/// Expected asset file name for a release (without path), e.g.
/// `leveler-v1.0.0-aarch64-apple-darwin.tar.xz`.
///
/// The release workflow builds exactly this name. Anything else is "no asset
/// for this host", never a fuzzy match.
pub fn release_asset_name(version: &Version, triple: &str) -> String {
    release_asset_names(version, triple)
        .into_iter()
        .next()
        .expect("every target has at least one archive format")
}

/// Asset names this host may install, most preferred first.
///
/// Unix hosts prefer `.tar.xz`; a release published before that format switch
/// carries only `.tar.gz`, and `leveler update --version <old tag>` must still
/// resolve it. Windows has one zip.
pub fn release_asset_names(version: &Version, triple: &str) -> Vec<String> {
    let ext = archive_extension(triple);
    let mut names = vec![format!("leveler-v{version}-{triple}.{ext}")];
    if ext == "tar.xz" {
        names.push(format!("leveler-v{version}-{triple}.tar.gz"));
    }
    names
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
    fn each_shipped_target_maps_to_exactly_one_primary_asset() {
        for (triple, ext) in [
            ("aarch64-apple-darwin", "tar.xz"),
            ("x86_64-apple-darwin", "tar.xz"),
            ("x86_64-unknown-linux-gnu", "tar.xz"),
            ("aarch64-unknown-linux-gnu", "tar.xz"),
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
        assert_eq!(archive_extension("aarch64-apple-darwin"), "tar.xz");
    }

    #[test]
    fn a_prerelease_tag_keeps_its_identifiers_in_the_asset_name() {
        assert_eq!(
            release_asset_name(&v("1.0.0-beta.2"), "aarch64-apple-darwin"),
            "leveler-v1.0.0-beta.2-aarch64-apple-darwin.tar.xz"
        );
    }
}
