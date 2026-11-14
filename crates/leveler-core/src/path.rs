//! Path presentation helpers.
//!
//! Windows canonicalization returns verbatim paths (`\\?\C:\...`). They are
//! correct for filesystem operations but noisy in user-facing output. This
//! module keeps the transformation at the display boundary: callers must keep
//! using the original path for I/O, identity checks, and child processes.

use std::borrow::Cow;
use std::path::Path;

/// Render a path for humans without changing its filesystem meaning.
///
/// On Windows, strips the verbatim prefix only for forms where the conversion
/// is safe: drive-letter paths and UNC shares. Device paths such as
/// `\\?\Volume{...}` keep their prefix because it is part of their identity.
/// Other platforms return the path unchanged.
pub fn display_path(path: &Path) -> Cow<'_, str> {
    #[cfg(windows)]
    {
        if let Some(raw) = path.to_str() {
            return simplify_verbatim(raw);
        }
        Cow::Owned(simplify_verbatim(&path.to_string_lossy()).into_owned())
    }
    #[cfg(not(windows))]
    {
        path.to_string_lossy()
    }
}

/// Pure transformation behind [`display_path`], available to tests on every
/// platform so the Windows syntax is covered by the cross-platform suite.
#[cfg(any(windows, test))]
fn simplify_verbatim(raw: &str) -> Cow<'_, str> {
    let Some(rest) = raw.strip_prefix(r"\\?\") else {
        return Cow::Borrowed(raw);
    };
    // `\\?\UNC\server\share` is the verbatim spelling of `\\server\share`.
    if let Some(unc) = rest
        .strip_prefix("UNC\\")
        .or_else(|| rest.strip_prefix("unc\\"))
        && !unc.is_empty()
    {
        return Cow::Owned(format!(r"\\{unc}"));
    }
    let bytes = rest.as_bytes();
    // Only `C:\...` is unambiguously equivalent. `C:` alone is drive-relative
    // and `Volume{...}`/`GLOBALROOT` are device identities; leave those alone.
    if bytes.len() >= 3 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':' && bytes[2] == b'\\' {
        return Cow::Owned(rest.to_string());
    }
    Cow::Borrowed(raw)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn verbatim_drive_paths_lose_only_the_prefix() {
        assert_eq!(
            simplify_verbatim(r"\\?\C:\Users\张三\a b\file.txt"),
            r"C:\Users\张三\a b\file.txt"
        );
        assert_eq!(simplify_verbatim(r"\\?\d:\work"), r"d:\work");
        assert_eq!(simplify_verbatim(r"\\?\C:\"), r"C:\");
    }

    #[test]
    fn verbatim_unc_paths_become_plain_unc_paths() {
        assert_eq!(
            simplify_verbatim(r"\\?\UNC\server\share\dir\file.txt"),
            r"\\server\share\dir\file.txt"
        );
        assert_eq!(
            simplify_verbatim(r"\\?\unc\server\share"),
            r"\\server\share"
        );
    }

    #[test]
    fn device_and_incomplete_paths_keep_the_prefix() {
        for raw in [
            r"\\?\Volume{6f8d4a2e-0000-0000-0000-100000000000}\dir",
            r"\\?\GLOBALROOT\Device\HarddiskVolume1",
            r"\\?\C:",
            r"\\?\UNC\",
            r"\\?\UNC",
            r"\\?\",
        ] {
            assert_eq!(simplify_verbatim(raw), raw);
        }
    }

    #[test]
    fn ordinary_paths_are_untouched() {
        for raw in [r"C:\repo", r"\\server\share\file", "/tmp/repo", ""] {
            assert_eq!(simplify_verbatim(raw), raw);
        }
    }

    #[test]
    fn display_path_keeps_non_verbatim_input_identical() {
        assert_eq!(display_path(Path::new("relative/repo")), "relative/repo");
        assert_eq!(display_path(Path::new("/tmp/repo")), "/tmp/repo");
    }
}
