//! The build-state contract for a verification check.
//!
//! A verification check answers exactly one question: does THIS source build?
//! Two mechanical facts decide whether its verdict is worth anything:
//!
//! 1. **It may not read build state another project owns.** An inherited
//!    `CARGO_TARGET_DIR` is the person's own build preference, and a shared one
//!    is typically outside the command's write scope: `cargo` then fails to
//!    create `<target>/debug` and a perfectly good tree "fails" verification.
//!    The same directory also carries artifacts of a different project, which a
//!    verdict must never absorb. So a check keeps an inherited target directory
//!    only when it resolves inside the workspace being verified (that is this
//!    workspace's own state) and otherwise uses a directory Leveler owns for
//!    this workspace, which stays warm across checks.
//!
//! 2. **A cache may not answer for a tree it never compiled.** Cargo decides
//!    whether a unit needs recompiling from file *mtimes*, not contents. A file
//!    whose content changed while its mtime did not move forward — a checkout,
//!    a restored backup, an editor's atomic save, a filesystem with a coarse
//!    clock — leaves cargo's fingerprint looking current, and the gate reports
//!    success for a broken tree. That is the one answer a coding agent cannot
//!    survive. So before the check runs, every input under the workspace is
//!    compared with the digest the last verification was bound to, and the ones
//!    whose content changed get an mtime newer than any artifact. Cargo then
//!    recompiles exactly what changed: the decision is content-bound, and the
//!    clock is no longer able to lie.
//!
//! Both facts are mechanical: a write scope, a set of digests, and file mtimes.

use std::collections::BTreeMap;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use sha2::{Digest, Sha256};
use tokio::process::Command;

/// This workspace's verification build output, inside the Leveler-owned tool
/// cache. Separate from the person's own target directory: a verdict never
/// depends on, and never invalidates, their cache.
pub(crate) const VERIFY_TARGET: &str = "verify-target";
/// Every verification of this workspace writes its input receipt here.
pub(crate) const VERIFY_STATE: &str = "verify";
/// The input digests the last verification of this workspace was bound to.
const INPUT_RECEIPT: &str = "verified-inputs.json";
/// Directory names that are never build inputs: version control metadata,
/// build output in the default layout, Leveler's own runtime state, and the
/// dependency trees of other toolchains.
const SKIPPED_DIRS: &[&str] = &[".git", "target", ".leveler", "node_modules"];

/// The build-state directory this verification check uses.
///
/// [`Path::starts_with`] on the resolved path, so a relative inherited setting
/// and a symlinked workspace are both judged by where they really land.
pub(crate) fn build_state_dir(
    inherited: Option<&Path>,
    workspace: &Path,
    owned: PathBuf,
) -> (PathBuf, bool) {
    match inherited {
        Some(dir) if dir.starts_with(workspace) => (dir.to_path_buf(), false),
        _ => (owned, true),
    }
}

/// Point the child at this check's build output.
///
/// Both names are set because they are the environment form of the same Cargo
/// setting, and the environment outranks every `.cargo/config.toml` — including
/// the host's global one, whose `[build] target-dir` may name a path the
/// command cannot write.
pub(crate) fn apply_build_state(cmd: &mut Command, target: &Path) {
    cmd.env("CARGO_TARGET_DIR", target);
    cmd.env("CARGO_BUILD_TARGET_DIR", target);
}

/// Bind the check to the current content of the source tree.
///
/// Returns how many inputs had to be marked as changed. `state_dir` holds the
/// receipt; it is deliberately independent of the build output directory, so a
/// check whose build went to a different target directory is still bound to
/// the same content.
pub(crate) fn bind_to_current_source(workspace: &Path, state_dir: &Path) -> Result<usize, String> {
    let current = digest_inputs(workspace)?;
    let receipt_path = state_dir.join(INPUT_RECEIPT);
    let previous: BTreeMap<String, String> = match std::fs::read(&receipt_path) {
        Ok(bytes) => serde_json::from_slice(&bytes).map_err(|error| {
            format!(
                "verification input receipt {} is unreadable: {error}",
                receipt_path.display()
            )
        })?,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => BTreeMap::new(),
        Err(error) => {
            return Err(format!(
                "read verification input receipt {}: {error}",
                receipt_path.display()
            ));
        }
    };

    let mut moved = 0usize;
    for (relative, digest) in &current {
        if previous.get(relative) == Some(digest) {
            continue;
        }
        let path = workspace.join(relative);
        // Windows refuses `set_times` on a read-only handle; the write access
        // requested there is only for the timestamp, never for the content.
        #[cfg(windows)]
        let opened = std::fs::OpenOptions::new().write(true).open(&path);
        #[cfg(not(windows))]
        let opened = std::fs::File::open(&path);
        let file = opened
            .map_err(|error| format!("open {} to bind the check to it: {error}", path.display()))?;
        // Only the timestamp moves: the content is exactly what the user wrote.
        // A source file that cannot be marked as changed is one this check
        // cannot prove it judged, so the check refuses to run instead of
        // reporting a verdict about it.
        file.set_times(std::fs::FileTimes::new().set_modified(SystemTime::now()))
            .map_err(|error| {
                format!(
                    "mark {} as changed for verification: {error}",
                    path.display()
                )
            })?;
        moved += 1;
    }

    std::fs::create_dir_all(state_dir).map_err(|error| {
        format!(
            "create verification state directory {}: {error}",
            state_dir.display()
        )
    })?;
    let encoded = serde_json::to_vec(&current).map_err(|error| error.to_string())?;
    std::fs::write(&receipt_path, encoded).map_err(|error| {
        format!(
            "write verification input receipt {}: {error}",
            receipt_path.display()
        )
    })?;
    Ok(moved)
}

/// Digest every regular file under `workspace` that a build could read.
///
/// The set is a superset of any compiler's inputs: an extra file only makes a
/// check recompile something it did not need to, never lets it skip something
/// it should have compiled. Keys are workspace-relative paths, so a receipt
/// stays valid when the tree is moved.
fn digest_inputs(workspace: &Path) -> Result<BTreeMap<String, String>, String> {
    let mut digests = BTreeMap::new();
    let mut pending = vec![workspace.to_path_buf()];
    while let Some(dir) = pending.pop() {
        let entries = match std::fs::read_dir(&dir) {
            Ok(entries) => entries,
            // An unreadable directory cannot be read by the compiler either,
            // so it contributes no inputs. The root is the caller's mistake and
            // is reported by the walk's first `read_dir`.
            Err(error) if dir == workspace => {
                return Err(format!("read workspace {}: {error}", workspace.display()));
            }
            Err(error) => {
                tracing::warn!(
                    directory = %dir.display(),
                    %error,
                    "verification input scan skipped an unreadable directory"
                );
                continue;
            }
        };
        for entry in entries {
            let entry = match entry {
                Ok(entry) => entry,
                Err(error) => {
                    tracing::warn!(directory = %dir.display(), %error, "verification input scan skipped an entry");
                    continue;
                }
            };
            let path = entry.path();
            let metadata = match std::fs::symlink_metadata(&path) {
                Ok(metadata) => metadata,
                Err(error) => {
                    tracing::warn!(file = %path.display(), %error, "verification input scan skipped an entry");
                    continue;
                }
            };
            if metadata.file_type().is_symlink() {
                // Followed, because a build follows it: a symlinked source file
                // is read through the link. A link leaving the workspace is not
                // part of this tree and is not hashed.
                let Ok(resolved) = std::fs::canonicalize(&path) else {
                    continue;
                };
                if !resolved.starts_with(workspace) || !resolved.is_file() {
                    continue;
                }
                let Some(relative) = relative_path(workspace, &resolved) else {
                    continue;
                };
                digests.insert(relative, sha256_file(&resolved)?);
                continue;
            }
            if metadata.is_dir() {
                let name = path
                    .file_name()
                    .and_then(|name| name.to_str())
                    .unwrap_or("");
                if SKIPPED_DIRS.contains(&name) {
                    continue;
                }
                pending.push(path);
                continue;
            }
            if !metadata.is_file() {
                continue;
            }
            let Some(relative) = relative_path(workspace, &path) else {
                continue;
            };
            digests.insert(relative, sha256_file(&path)?);
        }
    }
    Ok(digests)
}

/// A workspace-relative key with `/` separators, so one receipt is portable.
fn relative_path(workspace: &Path, path: &Path) -> Option<String> {
    let relative = path.strip_prefix(workspace).ok()?;
    Some(
        relative
            .components()
            .map(|component| component.as_os_str().to_string_lossy().into_owned())
            .collect::<Vec<_>>()
            .join("/"),
    )
}

fn sha256_file(path: &Path) -> Result<String, String> {
    let mut file = std::fs::File::open(path)
        .map_err(|error| format!("open {} for hashing: {error}", path.display()))?;
    let mut hasher = Sha256::new();
    let mut buffer = vec![0u8; 64 * 1024];
    loop {
        let read = file
            .read(&mut buffer)
            .map_err(|error| format!("read {} for hashing: {error}", path.display()))?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(format!("{:x}", hasher.finalize()))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A workspace holding two inputs, and a state directory that is outside
    /// it — the same placement the sandbox guarantees, so the receipt never
    /// becomes an input of the tree it describes.
    fn fixture() -> (tempfile::TempDir, PathBuf) {
        let base = tempfile::Builder::new()
            .prefix(".leveler-verify-inputs-")
            .tempdir()
            .unwrap();
        let workspace = base.path().join("workspace");
        std::fs::create_dir_all(workspace.join("src")).unwrap();
        std::fs::write(workspace.join("src/main.rs"), "fn main() {}\n").unwrap();
        std::fs::write(workspace.join("Cargo.toml"), "[package]\n").unwrap();
        (base, workspace)
    }

    /// The receipt records content, so an input whose mtime moved without its
    /// bytes changing needs no recompile — and one whose bytes changed without
    /// its mtime moving is exactly what must be recompiled.
    #[test]
    fn a_receipt_tracks_content_rather_than_the_clock() {
        let (base, workspace) = fixture();
        let workspace = workspace.as_path();
        let state = base.path().join("state");
        let first = bind_to_current_source(workspace, &state).unwrap();
        assert_eq!(first, 2, "every input is new to the first receipt");

        let again = bind_to_current_source(workspace, &state).unwrap();
        assert_eq!(again, 0, "unchanged content needs no recompilation");

        // A save that bumps the clock but not the bytes.
        let source = workspace.join("src/main.rs");
        let handle = std::fs::File::open(&source).unwrap();
        handle
            .set_times(std::fs::FileTimes::new().set_modified(SystemTime::now()))
            .unwrap();
        drop(handle);
        assert_eq!(bind_to_current_source(workspace, &state).unwrap(), 0);

        // A change that leaves the old mtime in place.
        let original = std::fs::metadata(&source).unwrap();
        std::fs::write(&source, "fn main() { this is not rust }\n").unwrap();
        let handle = std::fs::File::open(&source).unwrap();
        handle
            .set_times(std::fs::FileTimes::new().set_modified(original.modified().unwrap()))
            .unwrap();
        drop(handle);
        assert_eq!(
            bind_to_current_source(workspace, &state).unwrap(),
            1,
            "the changed input is the one that must be recompiled"
        );
    }

    /// Build output and version control metadata are not inputs: walking them
    /// would make a verification hash its own last build.
    #[test]
    fn build_output_and_version_control_are_not_inputs() {
        let (_base, workspace) = fixture();
        std::fs::create_dir_all(workspace.join("target/debug")).unwrap();
        std::fs::write(workspace.join("target/debug/artifact"), "x").unwrap();
        std::fs::create_dir_all(workspace.join(".git")).unwrap();
        std::fs::write(workspace.join(".git/HEAD"), "ref").unwrap();
        let digests = digest_inputs(&workspace).unwrap();
        let keys: Vec<&str> = digests.keys().map(String::as_str).collect();
        assert_eq!(keys, vec!["Cargo.toml", "src/main.rs"]);
    }

    /// A build state directory inside the workspace is that workspace's own and
    /// is kept; anything else is not this check's to depend on.
    #[test]
    fn only_a_target_dir_inside_the_workspace_is_kept() {
        let (base, workspace) = fixture();
        let workspace = workspace.as_path();
        let owned = base.path().join("cache").join(VERIFY_TARGET);
        assert_eq!(
            build_state_dir(Some(&workspace.join("target")), workspace, owned.clone()),
            (workspace.join("target"), false)
        );
        assert_eq!(
            build_state_dir(Some(&base.path().join("shared")), workspace, owned.clone()),
            (owned.clone(), true)
        );
        assert_eq!(
            build_state_dir(None, workspace, owned.clone()),
            (owned, true)
        );
    }
}
