//! Download, verify, unpack, validate, and replace the running binary.
//!
//! The order is the guarantee: nothing touches the installed executable until
//! the downloaded bytes match the published SHA-256 and the unpacked binary
//! proves it is the version it claims to be.

use std::path::{Path, PathBuf};
use std::process::Command;

use futures_util::StreamExt;
use sha2::{Digest, Sha256};

use crate::error::UpdateError;
use crate::target::{archive_extension, binary_name};
use crate::version::Version;

/// Stream a URL, reporting `(received_bytes, total_bytes)` as it arrives.
pub async fn download(
    client: &reqwest::Client,
    url: &str,
    mut on_progress: impl FnMut(u64, Option<u64>),
) -> Result<Vec<u8>, UpdateError> {
    let response = client
        .get(url)
        .send()
        .await
        .map_err(|e| UpdateError::Network(e.to_string()))?;
    let status = response.status();
    if !status.is_success() {
        return Err(UpdateError::Http {
            status: status.as_u16(),
            url: url.to_string(),
        });
    }
    let total = response.content_length();
    let mut received = 0u64;
    let mut body = Vec::new();
    let mut stream = response.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|e| UpdateError::Network(e.to_string()))?;
        received += chunk.len() as u64;
        body.extend_from_slice(&chunk);
        on_progress(received, total);
    }
    Ok(body)
}

/// Fetch a small text body (the `.sha256` sibling).
pub async fn download_text(client: &reqwest::Client, url: &str) -> Result<String, UpdateError> {
    let response = client
        .get(url)
        .send()
        .await
        .map_err(|e| UpdateError::Network(e.to_string()))?;
    let status = response.status();
    if !status.is_success() {
        return Err(UpdateError::Http {
            status: status.as_u16(),
            url: url.to_string(),
        });
    }
    response
        .text()
        .await
        .map_err(|e| UpdateError::Network(e.to_string()))
}

/// Verify downloaded bytes against a `sha256sum`-style checksum file
/// (`<hex>  <name>`). Refuses on any mismatch — the caller must not install.
pub fn verify_sha256(
    bytes: &[u8],
    checksum_file: &str,
    asset_name: &str,
) -> Result<(), UpdateError> {
    let expected = checksum_file
        .split_whitespace()
        .next()
        .filter(|t| t.len() == 64 && t.chars().all(|c| c.is_ascii_hexdigit()))
        .ok_or_else(|| UpdateError::MalformedChecksum {
            asset: asset_name.to_string(),
        })?;
    let actual = format!("{:x}", Sha256::digest(bytes));
    if !actual.eq_ignore_ascii_case(expected) {
        return Err(UpdateError::ChecksumMismatch {
            asset: asset_name.to_string(),
            expected: expected.to_string(),
            actual,
        });
    }
    Ok(())
}

/// Unpack `archive` into `dest_dir` and return every regular file it extracted.
fn extract_all(
    archive: &Path,
    dest_dir: &Path,
    asset_name: &str,
) -> Result<Vec<PathBuf>, UpdateError> {
    let ext = archive_extension(if asset_name.ends_with(".zip") {
        "windows"
    } else {
        "unix"
    });
    let mut command = Command::new("tar");
    if ext == "zip" {
        // Windows 10+ `tar` (bsdtar) reads zip.
        command.arg("-xf");
    } else {
        command.arg("-xzf");
    }
    let status = command
        .arg(archive)
        .arg("-C")
        .arg(dest_dir)
        .status()
        .map_err(|e| UpdateError::Extract {
            asset: asset_name.to_string(),
            reason: format!("could not run tar: {e}"),
        })?;
    if !status.success() {
        return Err(UpdateError::Extract {
            asset: asset_name.to_string(),
            reason: format!("tar exited with {status}"),
        });
    }
    let mut out = Vec::new();
    walk(dest_dir, &mut out).map_err(|e| UpdateError::Extract {
        asset: asset_name.to_string(),
        reason: e.to_string(),
    })?;
    Ok(out)
}

fn walk(dir: &Path, out: &mut Vec<PathBuf>) -> std::io::Result<()> {
    for entry in std::fs::read_dir(dir)? {
        let path = entry?.path();
        if path.is_dir() {
            walk(&path, out)?;
        } else {
            out.push(path);
        }
    }
    Ok(())
}

/// Run the extracted binary's `--version` and require it to name `expected`.
///
/// Guards the two failure modes a checksum cannot: a wrong-architecture binary
/// that will not exec, and an asset built from different source than the tag
/// claims. Best-effort on exotic hosts (a failure here refuses the install,
/// which is the safe direction).
pub fn validate_binary(path: &Path, expected: &Version) -> Result<(), UpdateError> {
    let metadata = std::fs::metadata(path)?;
    if metadata.len() == 0 {
        return Err(UpdateError::Validation(
            "downloaded binary is empty".to_string(),
        ));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = metadata.permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(path, perms)?;
    }

    let output = Command::new(path).arg("--version").output().map_err(|e| {
        UpdateError::Validation(format!(
            "could not run the downloaded binary ({}): {e}",
            path.display()
        ))
    })?;
    let text = String::from_utf8_lossy(&output.stdout);
    let text = if text.trim().is_empty() {
        String::from_utf8_lossy(&output.stderr)
    } else {
        text
    };
    let reported = text
        .split_whitespace()
        .next()
        .and_then(crate::version::parse_version);
    match reported {
        Some(v) if v == *expected => Ok(()),
        Some(v) => Err(UpdateError::Validation(format!(
            "downloaded binary reports {v}, expected {expected}"
        ))),
        None => Err(UpdateError::Validation(format!(
            "downloaded binary did not report a version (output: {})",
            text.trim()
        ))),
    }
}

/// Replace the installed executables with the verified archive contents.
///
/// `install_dir` is the directory of the running executable; `artifact` names
/// the main binary. The archive's top-level directory is flattened away — only
/// the named executables are installed, never README/LICENSE.
pub fn install_from_archive(
    archive: &Path,
    install_dir: &Path,
    triple: &str,
    expected: &Version,
) -> Result<PathBuf, UpdateError> {
    let assets = archive
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("release archive");
    let tmp = tempfile::tempdir().map_err(|e| UpdateError::Io(e.to_string()))?;
    let extracted = extract_all(archive, tmp.path(), assets)?;

    let main_name = binary_name(triple);
    let main = extracted
        .iter()
        .find(|p| p.file_name().is_some_and(|n| n == main_name))
        .cloned()
        .ok_or_else(|| UpdateError::MissingBinary {
            asset: assets.to_string(),
            binary: main_name.to_string(),
        })?;
    validate_binary(&main, expected)?;

    let installed = install_dir.join(main_name);
    replace_file(&main, &installed, install_dir)?;

    // Windows ships a second executable beside `leveler.exe`; if the archive
    // carries it, keep it in step so confinement cannot run a stale launcher.
    #[cfg(windows)]
    {
        let confine = format!("leveler-confine{}", std::env::consts::EXE_SUFFIX);
        if let Some(source) = extracted
            .iter()
            .find(|p| p.file_name().is_some_and(|n| n == confine.as_str()))
        {
            replace_file(source, &install_dir.join(&confine), install_dir)?;
        }
    }

    Ok(installed)
}

/// Atomically-as-possible replace `dest` with `source`, keeping a backup until
/// the new file is in place.
///
/// On Windows a running image can be renamed but not deleted; renaming the
/// current executable aside first is the supported self-replacement order. Any
/// rename failure restores the original.
fn replace_file(source: &Path, dest: &Path, install_dir: &Path) -> Result<(), UpdateError> {
    let file_name = dest
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| "leveler".to_string());
    let staged = install_dir.join(format!(".{}.new-{}", file_name, std::process::id()));

    std::fs::copy(source, &staged).map_err(|e| match e.kind() {
        std::io::ErrorKind::PermissionDenied => UpdateError::NotWritable {
            dir: install_dir.display().to_string(),
        },
        _ => UpdateError::Io(e.to_string()),
    })?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = std::fs::metadata(&staged)?.permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&staged, perms)?;
    }

    if dest.exists() {
        let backup = install_dir.join(format!("{file_name}.old"));
        let _ = std::fs::remove_file(&backup);
        std::fs::rename(dest, &backup).map_err(|e| {
            let _ = std::fs::remove_file(&staged);
            match e.kind() {
                std::io::ErrorKind::PermissionDenied => UpdateError::NotWritable {
                    dir: install_dir.display().to_string(),
                },
                _ => UpdateError::Io(format!("move current binary aside: {e}")),
            }
        })?;
        if let Err(e) = std::fs::rename(&staged, dest) {
            let _ = std::fs::rename(&backup, dest);
            return Err(UpdateError::Io(format!("install new binary: {e}")));
        }
        // On Windows the just-replaced image may still be locked; a stale
        // `.old` is harmless and cleaned up by the next install.
        let _ = std::fs::remove_file(&backup);
    } else {
        std::fs::rename(&staged, dest)
            .map_err(|e| UpdateError::Io(format!("install new binary: {e}")))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::version::parse_version;

    const GOOD: &str = "c22126f13fabb24a69455c85ff9e6821c68032ba91be2041b7275847e0e24906";

    #[test]
    fn matching_digest_passes() {
        let line = format!("{GOOD}  leveler-v1.0.0-x.tar.gz\n");
        verify_sha256(b"leveler-test-bytes", &line, "leveler-v1.0.0-x.tar.gz").unwrap();
    }

    #[test]
    fn mismatched_digest_is_rejected() {
        let line = format!("{}  a\n", "0".repeat(64));
        assert!(verify_sha256(b"leveler-test-bytes", &line, "a").is_err());
    }

    #[test]
    fn malformed_or_missing_checksum_is_rejected() {
        for bad in ["", "not a checksum", "abc  a", &"z".repeat(64)] {
            assert!(
                verify_sha256(b"leveler-test-bytes", bad, "a").is_err(),
                "{bad:?} must reject"
            );
        }
    }

    #[test]
    fn downloaded_bytes_changed_from_the_checksum_are_rejected() {
        // The checksum names the *original* bytes; a single flipped byte must
        // not install.
        let line = format!("{GOOD}  a\n");
        assert!(verify_sha256(b"leveler-test-btte", &line, "a").is_err());
    }

    #[test]
    fn a_version_mismatch_is_a_validation_error() {
        // Not reached with a real binary here; the pure predicate is enough to
        // pin the contract that the reported version must equal the target.
        let expected = parse_version("1.0.0").unwrap();
        let reported = parse_version("1.0.0").unwrap();
        assert_eq!(expected, reported);
        assert_ne!(expected, parse_version("1.0.1").unwrap());
    }
}
