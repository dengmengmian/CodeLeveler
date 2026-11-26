//! Content-addressed storage for large text artifacts (command output that
//! would otherwise be silently truncated). Mirrors the media store's
//! deduplicating pattern: identical content hashes to the same file, so a
//! preview shown to the model can always point at the full, retrievable output.

use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};

/// A stored artifact: where the full content lives, its hash, and its size.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArtifactRef {
    pub path: PathBuf,
    pub sha256: String,
    pub size_bytes: usize,
}

/// A content-addressed text-artifact store rooted at a directory (kept OUTSIDE
/// the workspace so artifacts never pollute the repo or verification scope).
///
/// The store is bounded: writing a new artifact sweeps the oldest files once
/// the directory exceeds `max_bytes`. A reference to a swept file is honestly
/// unavailable rather than silently pointing at different bytes (dedup means a
/// surviving file never changes content).
#[derive(Debug, Clone)]
pub struct ArtifactStore {
    root: PathBuf,
    max_bytes: u64,
}

/// 512 MiB. Large enough for a long session's paged command output, small
/// enough that it cannot silently consume a disk.
pub const DEFAULT_ARTIFACT_QUOTA_BYTES: u64 = 512 * 1024 * 1024;

/// Why a stored artifact could not be served to a reader.
#[derive(Debug)]
pub enum ArtifactReadError {
    /// The path is not a direct child of this store, or is not a
    /// content-addressed `<sha256>.txt` file. The reader refuses to touch
    /// anything else, so a forged reference can never become an arbitrary
    /// local file read.
    NotArtifact,
    /// The file no longer exists (swept by the quota, or deleted).
    Missing,
    /// The requested offset is beyond the end of the file.
    OffsetOutOfRange { total_bytes: u64 },
    /// The file exists but could not be read.
    Io(std::io::Error),
}

impl std::fmt::Display for ArtifactReadError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotArtifact => write!(f, "not an artifact in this store"),
            Self::Missing => write!(f, "artifact file is missing"),
            Self::OffsetOutOfRange { total_bytes } => {
                write!(f, "offset is beyond the artifact ({total_bytes} bytes)")
            }
            Self::Io(error) => write!(f, "artifact read failed: {error}"),
        }
    }
}

impl std::error::Error for ArtifactReadError {}

/// One page of an artifact's text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArtifactPage {
    pub text: String,
    pub offset: u64,
    pub next_offset: u64,
    pub total_bytes: u64,
    pub has_more: bool,
}

impl ArtifactStore {
    /// The directory this store owns. Callers may compare it for display, but
    /// a read path must still resolve through [`Self::resolve`].
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Resolve `path` to a regular file this store wrote.
    ///
    /// The name must be a 64-hex-digit content hash plus `.txt` and the parent
    /// must be this store's canonical root. Symlinks are refused on Unix via
    /// `O_NOFOLLOW` at open time and by the metadata check here, so a link
    /// planted inside the store cannot redirect the read.
    pub fn resolve(&self, path: &Path) -> Result<PathBuf, ArtifactReadError> {
        let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
            return Err(ArtifactReadError::NotArtifact);
        };
        if !is_content_addressed_name(name) {
            return Err(ArtifactReadError::NotArtifact);
        }
        let root = self.root.canonicalize().map_err(|error| {
            if error.kind() == std::io::ErrorKind::NotFound {
                ArtifactReadError::Missing
            } else {
                ArtifactReadError::Io(error)
            }
        })?;
        let parent = path
            .parent()
            .ok_or(ArtifactReadError::NotArtifact)?
            .canonicalize()
            .map_err(|error| {
                if error.kind() == std::io::ErrorKind::NotFound {
                    ArtifactReadError::Missing
                } else {
                    ArtifactReadError::Io(error)
                }
            })?;
        if parent != root {
            return Err(ArtifactReadError::NotArtifact);
        }
        let metadata = std::fs::symlink_metadata(path).map_err(|error| {
            if error.kind() == std::io::ErrorKind::NotFound {
                ArtifactReadError::Missing
            } else {
                ArtifactReadError::Io(error)
            }
        })?;
        if !metadata.file_type().is_file() || metadata.file_type().is_symlink() {
            return Err(ArtifactReadError::NotArtifact);
        }
        Ok(path.to_path_buf())
    }

    /// Size of a resolvable artifact.
    pub fn size_of(&self, path: &Path) -> Result<u64, ArtifactReadError> {
        let resolved = self.resolve(path)?;
        std::fs::metadata(&resolved)
            .map(|metadata| metadata.len())
            .map_err(ArtifactReadError::Io)
    }

    /// Read at most `max_bytes` from `offset`, never splitting a UTF-8
    /// character. The returned `next_offset` is the exact continuation point,
    /// so a client can page the whole file without loss.
    pub fn read_page(
        &self,
        path: &Path,
        offset: u64,
        max_bytes: usize,
    ) -> Result<ArtifactPage, ArtifactReadError> {
        let resolved = self.resolve(path)?;
        let mut file = open_no_follow(&resolved).map_err(|error| {
            if error.kind() == std::io::ErrorKind::NotFound {
                ArtifactReadError::Missing
            } else {
                ArtifactReadError::Io(error)
            }
        })?;
        let total_bytes = file.metadata().map_err(ArtifactReadError::Io)?.len();
        if offset > total_bytes {
            return Err(ArtifactReadError::OffsetOutOfRange { total_bytes });
        }
        if max_bytes == 0 {
            return Ok(ArtifactPage {
                text: String::new(),
                offset,
                next_offset: offset,
                total_bytes,
                has_more: offset < total_bytes,
            });
        }
        file.seek(SeekFrom::Start(offset))
            .map_err(ArtifactReadError::Io)?;
        // One extra char's worth so a cut landing mid-character can still be
        // completed by the next page.
        let want = max_bytes.saturating_add(4);
        let mut buf = vec![0u8; want];
        let mut read = 0usize;
        while read < buf.len() {
            match file.read(&mut buf[read..]) {
                Ok(0) => break,
                Ok(n) => read += n,
                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(error) => return Err(ArtifactReadError::Io(error)),
            }
        }
        buf.truncate(read);
        // Cut at a character boundary. Our own writes are always valid UTF-8,
        // so a split can only happen at the page edge; anything else is a
        // corrupted file and is served lossily rather than failing the read.
        let valid = match std::str::from_utf8(&buf) {
            Ok(_) => buf.len(),
            Err(error) => error.valid_up_to().max(1).min(buf.len()),
        };
        let text = String::from_utf8_lossy(&buf[..valid]).into_owned();
        let next_offset = offset.saturating_add(valid as u64);
        Ok(ArtifactPage {
            text,
            offset,
            next_offset,
            total_bytes,
            has_more: next_offset < total_bytes,
        })
    }
}

/// `<sha256>.txt`, lowercase hex only. Nothing else can be an artifact name.
fn is_content_addressed_name(name: &str) -> bool {
    let Some(hash) = name.strip_suffix(".txt") else {
        return false;
    };
    hash.len() == 64
        && hash
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

#[cfg(unix)]
fn open_no_follow(path: &Path) -> std::io::Result<File> {
    use std::os::unix::fs::OpenOptionsExt;
    std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(nix::libc::O_NOFOLLOW)
        .open(path)
}

#[cfg(not(unix))]
fn open_no_follow(path: &Path) -> std::io::Result<File> {
    // Windows has no O_NOFOLLOW, and the file type check above (not a symlink)
    // is the boundary there.
    File::open(path)
}

impl ArtifactStore {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self {
            root: root.into(),
            max_bytes: DEFAULT_ARTIFACT_QUOTA_BYTES,
        }
    }

    /// Set the directory quota (bytes). 0 disables cleanup and is refused.
    pub fn with_max_bytes(mut self, max_bytes: u64) -> Self {
        self.max_bytes = max_bytes.max(1);
        self
    }

    /// Write `content` to a content-addressed file (`<sha256>.txt`). Identical
    /// content deduplicates to the same path and is not rewritten.
    ///
    /// Secrets are redacted before hashing/writing so artifacts never store
    /// raw API keys or Authorization headers. The redaction is the
    /// value-position aware one (`sanitize_model_visible`), not the old
    /// whole-line keyword scrubber: an artifact is command output the model
    /// reads back, so rewriting `let password = config.password;` into
    /// `let password = [REDACTED];` is irreversible corruption of the only
    /// copy of that output.
    pub fn write_text(&self, content: &str) -> std::io::Result<ArtifactRef> {
        let content = leveler_core::sanitize_model_visible(content).0;
        let sha256 = hex(&Sha256::digest(content.as_bytes()));
        std::fs::create_dir_all(&self.root)?;
        let path = self.root.join(format!("{sha256}.txt"));
        if !path.exists() {
            std::fs::write(&path, &content)?;
        }
        self.sweep(&path);
        Ok(ArtifactRef {
            path,
            sha256,
            size_bytes: content.len(),
        })
    }

    /// Delete oldest artifacts until the directory fits the quota.
    ///
    /// Best-effort by design: a failed delete leaves the directory over quota
    /// rather than failing the tool call whose output was already produced.
    /// The file just written is never a deletion candidate — the caller holds a
    /// live reference to it.
    fn sweep(&self, keep: &std::path::Path) {
        let Ok(entries) = std::fs::read_dir(&self.root) else {
            return;
        };
        let mut files: Vec<(std::time::SystemTime, u64, PathBuf)> = Vec::new();
        let mut total = 0u64;
        for entry in entries.flatten() {
            let path = entry.path();
            let Ok(meta) = entry.metadata() else {
                continue;
            };
            if !meta.is_file() || path == keep {
                total = total.saturating_add(meta.len());
                continue;
            }
            total = total.saturating_add(meta.len());
            files.push((
                meta.modified().unwrap_or(std::time::UNIX_EPOCH),
                meta.len(),
                path,
            ));
        }
        if total <= self.max_bytes {
            return;
        }
        files.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| a.2.cmp(&b.2)));
        for (_, size, path) in files {
            if total <= self.max_bytes {
                break;
            }
            match std::fs::remove_file(&path) {
                Ok(()) => total = total.saturating_sub(size),
                Err(error) => tracing::debug!(
                    %error,
                    path = %path.display(),
                    "artifact quota sweep could not remove a file"
                ),
            }
        }
    }
}

fn hex(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push_str(&format!("{b:02x}"));
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp() -> PathBuf {
        // A per-process atomic counter — NOT a timestamp. Windows' coarse timer
        // resolution (~15ms) let parallel tests collide on the same nanos → same
        // dir, and one test's remove_dir_all raced another's write (Windows
        // forbids deleting a dir with live handles). The counter is unique per
        // call regardless of clock resolution; process id disambiguates across
        // processes.
        use std::sync::atomic::{AtomicU64, Ordering};
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        std::env::temp_dir().join(format!(
            "leveler-artifact-test-{}-{}",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::Relaxed)
        ))
    }

    #[test]
    fn writes_full_content_and_reports_hash_and_size() {
        let root = tmp();
        let store = ArtifactStore::new(&root);
        let content = "a".repeat(100_000);
        let art = store.write_text(&content).unwrap();

        assert_eq!(art.size_bytes, 100_000);
        assert!(art.path.starts_with(&root));
        assert_eq!(std::fs::read_to_string(&art.path).unwrap(), content);
        std::fs::remove_dir_all(&root).ok();
    }

    /// The store is bounded: once the quota is exceeded the oldest files are
    /// removed, and the newly written artifact (the caller's live reference)
    /// always survives. A swept reference is honestly missing, never a
    /// different file with the same name.
    #[test]
    fn the_artifact_quota_sweeps_oldest_files_but_keeps_the_new_one() {
        let root = tmp();
        let store = ArtifactStore::new(&root).with_max_bytes(120);
        let first = store.write_text(&"a".repeat(60)).unwrap();
        let second = store.write_text(&"b".repeat(60)).unwrap();
        let third = store.write_text(&"c".repeat(60)).unwrap();
        assert!(
            third.path.is_file(),
            "the newly written artifact is never a sweep candidate"
        );
        assert!(second.path.is_file());
        assert!(
            !first.path.is_file(),
            "the oldest artifact is removed to stay within the quota"
        );
        let total: u64 = std::fs::read_dir(&root)
            .unwrap()
            .flatten()
            .filter_map(|entry| entry.metadata().ok())
            .map(|meta| meta.len())
            .sum();
        assert!(total <= 120, "quota exceeded: {total}");
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn redacts_secrets_before_writing() {
        let root = tmp();
        let store = ArtifactStore::new(&root);
        let art = store
            .write_text("Authorization: Bearer sk-abcdefghijklmnop1234")
            .unwrap();
        let stored = std::fs::read_to_string(&art.path).unwrap();
        assert!(!stored.contains("sk-abcdefghijklmnop1234"), "{stored}");
        assert!(stored.contains("[REDACTED]"), "{stored}");
        std::fs::remove_dir_all(&root).ok();
    }

    /// An artifact is command output the model may read back. Ordinary source
    /// code that mentions a credential word must survive byte-identical; only
    /// the credential VALUE is redacted. The old whole-line keyword scrubber
    /// rewrote `let password = config.password;` — irreversible corruption of
    /// the artifact's only copy.
    #[test]
    fn source_code_in_an_artifact_is_not_rewritten() {
        let root = tmp();
        let store = ArtifactStore::new(&root);
        let source = "pub struct Creds {\n    pub password: String,\n}\n\nfn load(config: &Config) -> String {\n    let password = config.password;\n    password\n}\n";
        let art = store.write_text(source).unwrap();
        let stored = std::fs::read_to_string(&art.path).unwrap();
        assert_eq!(stored, source, "artifact code was rewritten");
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn identical_content_deduplicates_to_the_same_path() {
        let root = tmp();
        let store = ArtifactStore::new(&root);
        let a = store.write_text("same output").unwrap();
        let b = store.write_text("same output").unwrap();
        assert_eq!(a.path, b.path, "identical content must dedup");
        assert_eq!(a.sha256, b.sha256);

        let c = store.write_text("different").unwrap();
        assert_ne!(c.path, a.path);
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn paging_returns_the_exact_text_and_never_splits_chinese_characters() {
        let root = tmp();
        let store = ArtifactStore::new(&root);
        let content = "中文输出\n".repeat(500);
        let art = store.write_text(&content).unwrap();

        let mut joined = String::new();
        let mut offset = 0u64;
        loop {
            let page = store.read_page(&art.path, offset, 7).unwrap();
            assert_eq!(page.offset, offset);
            assert_eq!(page.total_bytes, content.len() as u64);
            assert!(page.text.is_char_boundary(0));
            joined.push_str(&page.text);
            if !page.has_more {
                assert_eq!(
                    page.next_offset, page.total_bytes,
                    "the last page ends the file"
                );
                break;
            }
            assert!(page.next_offset > offset, "a page must make progress");
            offset = page.next_offset;
        }
        assert_eq!(joined, content, "paged reads must be lossless");
        assert_eq!(store.size_of(&art.path).unwrap(), content.len() as u64);
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn a_path_outside_the_store_or_with_a_foreign_name_is_never_read() {
        let root = tmp();
        let store = ArtifactStore::new(&root);
        std::fs::create_dir_all(&root).unwrap();
        let outside = root.with_extension("outside");
        std::fs::create_dir_all(&outside).unwrap();
        let name = "a".repeat(64);
        let foreign = outside.join(format!("{name}.txt"));
        std::fs::write(&foreign, "not yours").unwrap();

        assert!(matches!(
            store.read_page(&foreign, 0, 1024),
            Err(ArtifactReadError::NotArtifact)
        ));
        // A real file inside the store with a non-artifact name is refused too.
        let art = store.write_text("inside").unwrap();
        let renamed = art.path.with_file_name("notes.txt");
        std::fs::write(&renamed, "inside").unwrap();
        assert!(matches!(
            store.read_page(&renamed, 0, 1024),
            Err(ArtifactReadError::NotArtifact)
        ));
        assert!(matches!(
            store.read_page(&art.path, art.size_bytes as u64 + 1, 1024),
            Err(ArtifactReadError::OffsetOutOfRange { .. })
        ));
        std::fs::remove_dir_all(&root).ok();
        std::fs::remove_dir_all(&outside).ok();
    }

    #[test]
    fn a_swept_artifact_is_reported_missing_not_served_as_empty() {
        let root = tmp();
        let store = ArtifactStore::new(&root).with_max_bytes(1);
        let art = store.write_text("will be swept by the next write").unwrap();
        // A second write exceeds the 1-byte quota and sweeps the first file.
        let keep = store.write_text("survivor").unwrap();
        assert!(keep.path.is_file());
        assert!(
            !art.path.exists(),
            "the test needs a swept artifact to prove the missing state"
        );
        assert!(matches!(
            store.read_page(&art.path, 0, 1024),
            Err(ArtifactReadError::Missing)
        ));
        std::fs::remove_dir_all(&root).ok();
    }

    #[cfg(unix)]
    #[test]
    fn a_symlink_named_like_an_artifact_is_refused() {
        let root = tmp();
        let store = ArtifactStore::new(&root);
        std::fs::create_dir_all(&root).unwrap();
        let outside = root.with_extension("symlink-target");
        std::fs::write(&outside, "secret").unwrap();
        let name = "b".repeat(64);
        let link = root.join(format!("{name}.txt"));
        std::os::unix::fs::symlink(&outside, &link).unwrap();
        assert!(matches!(
            store.read_page(&link, 0, 1024),
            Err(ArtifactReadError::NotArtifact)
        ));
        std::fs::remove_dir_all(&root).ok();
        std::fs::remove_file(&outside).ok();
    }
}
