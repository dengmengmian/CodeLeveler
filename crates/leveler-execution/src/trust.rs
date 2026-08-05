//! Trust gate for in-repo executable configuration.
//!
//! `<repo>/.leveler/hooks.yaml` and `<repo>/.leveler/permissions.yaml` are
//! files a *repository* can carry: hooks spawn arbitrary commands before every
//! tool call, and permission rules can grant standing `Allow` that skips the
//! approval prompt entirely. Cloning an untrusted repository must therefore not
//! be enough to make either take effect.
//!
//! The gate is content-addressed, not path-addressed: trusting a file records
//! the SHA-256 of the exact bytes the user saw. Editing the file afterwards —
//! by a teammate, by `git pull`, or by the agent itself — invalidates the
//! record, so the next run falls back to untrusted.
//!
//! Every failure mode resolves to *untrusted*: a missing store, an unreadable
//! store, corrupt YAML, or a path that will not canonicalize.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// In-repo files that execute or grant, relative to the repository root.
/// Order is the order `leveler trust` presents them.
pub const TRUSTED_PROJECT_FILES: [&str; 2] = [".leveler/hooks.yaml", ".leveler/permissions.yaml"];

/// An in-repo config file that was present but not trusted, so it was skipped.
/// Reported so the user learns it did not take effect — a silent skip looks
/// identical to "my hooks are broken".
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UntrustedConfig {
    pub path: PathBuf,
    pub digest: String,
}

/// Result of reading an in-repo executable config through the gate.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TrustedRead {
    /// The file does not exist (or could not be read at all).
    Absent,
    /// Present, and its exact bytes are trusted for this repository.
    Trusted(String),
    /// Present but not trusted — the caller must ignore it and may report it.
    Untrusted { path: PathBuf, digest: String },
}

#[derive(Debug, thiserror::Error)]
pub enum TrustError {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("serialize trust store: {0}")]
    Serialize(String),
}

/// `<global_home>/trusted.yaml`: repository → relative file → content digest.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct TrustFile {
    #[serde(default)]
    repos: BTreeMap<String, BTreeMap<String, String>>,
}

/// The persisted set of trusted in-repo config files.
#[derive(Debug, Clone)]
pub struct TrustStore {
    path: PathBuf,
    file: TrustFile,
}

/// Hex SHA-256 of the exact file bytes.
pub fn content_digest(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    hasher
        .finalize()
        .iter()
        .fold(String::with_capacity(64), |mut out, b| {
            use std::fmt::Write as _;
            let _ = write!(out, "{b:02x}");
            out
        })
}

/// Stable key for a repository. Canonicalized so `.`/symlink spellings of the
/// same checkout share one entry; falls back to the lexical path when the
/// repository does not exist yet.
fn repo_key(repo_root: &Path) -> String {
    repo_root
        .canonicalize()
        .unwrap_or_else(|_| repo_root.to_path_buf())
        .display()
        .to_string()
}

pub fn trust_store_path(global_home: &Path) -> PathBuf {
    global_home.join("trusted.yaml")
}

/// Whether the trust store sits outside the repository it grants trust for.
///
/// `leveler_home_dir_from` yields `None` with no HOME/USERPROFILE and callers
/// fall back to the relative `.leveler`, which resolves inside the repository
/// when leveler runs from the repo root. A store the repository can ship would
/// let a clone approve its own hooks, so that arrangement grants nothing.
pub fn store_is_outside_repo(global_home: &Path, repo_root: &Path) -> bool {
    let home = global_home
        .canonicalize()
        .unwrap_or_else(|_| global_home.to_path_buf());
    let repo = repo_root
        .canonicalize()
        .unwrap_or_else(|_| repo_root.to_path_buf());
    !home.starts_with(&repo)
}

impl TrustStore {
    /// Load the store. Any read or parse failure yields an empty store, which
    /// trusts nothing — the safe direction.
    pub fn load(global_home: &Path) -> Self {
        let path = trust_store_path(global_home);
        let file = std::fs::read_to_string(&path)
            .ok()
            .and_then(|raw| serde_yaml::from_str::<TrustFile>(&raw).ok())
            .unwrap_or_default();
        Self { path, file }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Whether `relative` in `repo_root` is trusted **with exactly these bytes**.
    pub fn is_trusted(&self, repo_root: &Path, relative: &str, content: &[u8]) -> bool {
        self.file
            .repos
            .get(&repo_key(repo_root))
            .and_then(|files| files.get(relative))
            .is_some_and(|recorded| *recorded == content_digest(content))
    }

    /// Record `relative` in `repo_root` as trusted at its current bytes.
    pub fn trust(&mut self, repo_root: &Path, relative: &str, content: &[u8]) {
        self.file
            .repos
            .entry(repo_key(repo_root))
            .or_default()
            .insert(relative.to_string(), content_digest(content));
    }

    /// Drop every record for `repo_root`. Returns whether anything was removed.
    pub fn revoke(&mut self, repo_root: &Path) -> bool {
        self.file.repos.remove(&repo_key(repo_root)).is_some()
    }

    pub fn save(&self) -> Result<(), TrustError> {
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let yaml =
            serde_yaml::to_string(&self.file).map_err(|e| TrustError::Serialize(e.to_string()))?;
        std::fs::write(&self.path, yaml)?;
        Ok(())
    }
}

/// Read an in-repo executable config through the trust gate.
///
/// The bytes are read once and both hashed and returned from that same buffer,
/// so what is checked is what the caller parses.
pub fn read_trusted_project_file(
    global_home: &Path,
    repo_root: &Path,
    relative: &str,
) -> TrustedRead {
    let path = repo_root.join(relative);
    let Ok(raw) = std::fs::read_to_string(&path) else {
        return TrustedRead::Absent;
    };
    if !store_is_outside_repo(global_home, repo_root) {
        // The store lives inside the repository, so the repository could ship
        // its own approval. Treat everything as untrusted.
        return TrustedRead::Untrusted {
            path,
            digest: content_digest(raw.as_bytes()),
        };
    }
    if TrustStore::load(global_home).is_trusted(repo_root, relative, raw.as_bytes()) {
        return TrustedRead::Trusted(raw);
    }
    TrustedRead::Untrusted {
        path,
        digest: content_digest(raw.as_bytes()),
    }
}

/// Every in-repo config file that is present but not trusted, for the startup
/// notice and `leveler trust`.
pub fn untrusted_project_files(global_home: &Path, repo_root: &Path) -> Vec<UntrustedConfig> {
    TRUSTED_PROJECT_FILES
        .iter()
        .filter_map(
            |relative| match read_trusted_project_file(global_home, repo_root, relative) {
                TrustedRead::Untrusted { path, digest } => Some(UntrustedConfig { path, digest }),
                TrustedRead::Absent | TrustedRead::Trusted(_) => None,
            },
        )
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    /// `leveler_home_dir_from` returns `None` when neither HOME nor USERPROFILE
    /// is set, and callers fall back to the relative `.leveler` — which lands
    /// *inside* the repository when leveler runs from the repo root. A store a
    /// repository can ship is not a store: refuse it rather than let a clone
    /// pre-approve its own hooks.
    #[test]
    fn a_trust_store_inside_the_repository_is_refused() {
        let repo = tempdir().unwrap();
        let home = repo.path().join(".leveler");
        let body = "hooks: {}\n";
        write(&repo.path().join(".leveler/hooks.yaml"), body);

        let mut store = TrustStore::load(&home);
        store.trust(repo.path(), ".leveler/hooks.yaml", body.as_bytes());
        store.save().unwrap();

        let read = read_trusted_project_file(&home, repo.path(), ".leveler/hooks.yaml");
        assert!(
            matches!(read, TrustedRead::Untrusted { .. }),
            "a repo-carried trust store must not grant trust: {read:?}"
        );
    }

    #[test]
    fn untrusted_project_files_lists_only_present_and_untrusted() {
        let home = tempdir().unwrap();
        let repo = tempdir().unwrap();
        let hooks = "hooks: {}\n";
        write(&repo.path().join(".leveler/hooks.yaml"), hooks);
        write(
            &repo.path().join(".leveler/permissions.yaml"),
            "rules: []\n",
        );

        let mut store = TrustStore::load(home.path());
        store.trust(repo.path(), ".leveler/hooks.yaml", hooks.as_bytes());
        store.save().unwrap();

        let listed = untrusted_project_files(home.path(), repo.path());
        assert_eq!(listed.len(), 1, "{listed:?}");
        assert!(listed[0].path.ends_with(".leveler/permissions.yaml"));
    }

    fn write(path: &Path, body: &str) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, body).unwrap();
    }

    #[test]
    fn an_unrecorded_file_is_not_trusted() {
        let home = tempdir().unwrap();
        let repo = tempdir().unwrap();
        write(&repo.path().join(".leveler/hooks.yaml"), "hooks: {}\n");

        let read = read_trusted_project_file(home.path(), repo.path(), ".leveler/hooks.yaml");
        assert!(matches!(read, TrustedRead::Untrusted { .. }), "{read:?}");
    }

    #[test]
    fn trusting_then_reading_the_same_bytes_is_trusted() {
        let home = tempdir().unwrap();
        let repo = tempdir().unwrap();
        let body = "hooks: {}\n";
        write(&repo.path().join(".leveler/hooks.yaml"), body);

        let mut store = TrustStore::load(home.path());
        store.trust(repo.path(), ".leveler/hooks.yaml", body.as_bytes());
        store.save().unwrap();

        assert_eq!(
            read_trusted_project_file(home.path(), repo.path(), ".leveler/hooks.yaml"),
            TrustedRead::Trusted(body.to_string())
        );
    }

    #[test]
    fn editing_a_trusted_file_revokes_trust() {
        let home = tempdir().unwrap();
        let repo = tempdir().unwrap();
        let path = repo.path().join(".leveler/hooks.yaml");
        write(&path, "hooks: {}\n");

        let mut store = TrustStore::load(home.path());
        store.trust(repo.path(), ".leveler/hooks.yaml", b"hooks: {}\n");
        store.save().unwrap();

        // The whole point of content addressing: a later edit must not inherit
        // the approval the user gave to the earlier bytes.
        write(
            &path,
            "hooks:\n  pre_tool_use:\n    - command: [\"sh\", \"-c\", \"id\"]\n",
        );
        let read = read_trusted_project_file(home.path(), repo.path(), ".leveler/hooks.yaml");
        assert!(matches!(read, TrustedRead::Untrusted { .. }), "{read:?}");
    }

    #[test]
    fn trust_does_not_leak_across_repositories() {
        let home = tempdir().unwrap();
        let trusted = tempdir().unwrap();
        let other = tempdir().unwrap();
        let body = "hooks: {}\n";
        write(&trusted.path().join(".leveler/hooks.yaml"), body);
        write(&other.path().join(".leveler/hooks.yaml"), body);

        let mut store = TrustStore::load(home.path());
        store.trust(trusted.path(), ".leveler/hooks.yaml", body.as_bytes());
        store.save().unwrap();

        assert!(matches!(
            read_trusted_project_file(home.path(), other.path(), ".leveler/hooks.yaml"),
            TrustedRead::Untrusted { .. }
        ));
    }

    #[test]
    fn a_corrupt_store_trusts_nothing() {
        let home = tempdir().unwrap();
        let repo = tempdir().unwrap();
        let body = "hooks: {}\n";
        write(&repo.path().join(".leveler/hooks.yaml"), body);
        std::fs::write(trust_store_path(home.path()), "{not: [valid").unwrap();

        assert!(matches!(
            read_trusted_project_file(home.path(), repo.path(), ".leveler/hooks.yaml"),
            TrustedRead::Untrusted { .. }
        ));
    }

    #[test]
    fn a_missing_file_is_absent_not_untrusted() {
        let home = tempdir().unwrap();
        let repo = tempdir().unwrap();
        assert_eq!(
            read_trusted_project_file(home.path(), repo.path(), ".leveler/hooks.yaml"),
            TrustedRead::Absent
        );
    }

    #[test]
    fn revoke_drops_every_file_for_the_repo() {
        let home = tempdir().unwrap();
        let repo = tempdir().unwrap();
        let mut store = TrustStore::load(home.path());
        store.trust(repo.path(), ".leveler/hooks.yaml", b"a");
        store.trust(repo.path(), ".leveler/permissions.yaml", b"b");
        assert!(store.revoke(repo.path()));
        assert!(!store.is_trusted(repo.path(), ".leveler/hooks.yaml", b"a"));
        assert!(!store.revoke(repo.path()));
    }
}
