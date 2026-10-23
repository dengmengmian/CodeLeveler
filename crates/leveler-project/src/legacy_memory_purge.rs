//! Precise removal of the retired JSON memory stores.
//!
//! CodeLeveler had one JSON memory store per project, under
//! `<state>/projects/<project>/memory/`. Memory 2.0 replaced it with Markdown,
//! and the product decision is that the old data is deleted, not migrated.
//!
//! This module exists so that deletion is a reviewed, testable operation
//! instead of `rm -rf ~/.leveler`:
//!
//! - Only a directory that IS a legacy store is considered: a real (not
//!   symlinked) directory named `memory` directly inside a project state
//!   directory or the no-workspace state directory.
//! - Its whole content must be recognized as legacy store artifacts. Anything
//!   unknown — an extra directory, a Markdown record, a symlink — blocks that
//!   store from deletion and is reported instead of guessed.
//! - Paths are canonicalized and must stay inside the canonical state root.
//! - A dry run is the default; nothing is written unless `apply` is true.
//!
//! It never touches Markdown current records, the private `memory-v2` state,
//! sessions, conversation logs, configuration, or repository files.

use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};

/// Legacy store subdirectories, by the names the JSON store created.
const LEGACY_DIRS: [&str; 4] = ["active", "archive", "pending", "suppress"];
/// Files the JSON store wrote at the store root.
const LEGACY_FILES: [&str; 2] = ["pending_history.jsonl", ".lifecycle.lock"];

#[derive(Debug)]
pub enum PurgeError {
    Io {
        path: PathBuf,
        source: std::io::Error,
    },
    Refused {
        path: PathBuf,
        reason: String,
    },
}

impl std::fmt::Display for PurgeError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io { path, source } => {
                write!(formatter, "cannot read {}: {source}", path.display())
            }
            Self::Refused { path, reason } => {
                write!(formatter, "refusing {}: {reason}", path.display())
            }
        }
    }
}

impl std::error::Error for PurgeError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io { source, .. } => Some(source),
            Self::Refused { .. } => None,
        }
    }
}

impl PurgeError {
    fn io(path: impl Into<PathBuf>, source: std::io::Error) -> Self {
        Self::Io {
            path: path.into(),
            source,
        }
    }
}

/// What one candidate store turned out to be.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StoreVerdict {
    /// Every entry is a known legacy artifact; `files`/`bytes` are what would
    /// be removed (or was removed when `apply` was set).
    LegacyStore { files: usize, bytes: u64 },
    /// Refused: the directory contains something this tool does not own.
    Blocked { reason: String },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoreReport {
    pub path: PathBuf,
    pub verdict: StoreVerdict,
    /// True only when `apply` was set and the directory is gone afterwards.
    pub removed: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PurgeReport {
    pub stores: Vec<StoreReport>,
}

impl PurgeReport {
    pub fn removed_stores(&self) -> usize {
        self.stores.iter().filter(|store| store.removed).count()
    }

    pub fn removed_files(&self) -> usize {
        self.stores
            .iter()
            .filter(|store| store.removed)
            .map(|store| match store.verdict {
                StoreVerdict::LegacyStore { files, .. } => files,
                StoreVerdict::Blocked { .. } => 0,
            })
            .sum()
    }

    pub fn removed_bytes(&self) -> u64 {
        self.stores
            .iter()
            .filter(|store| store.removed)
            .map(|store| match store.verdict {
                StoreVerdict::LegacyStore { bytes, .. } => bytes,
                StoreVerdict::Blocked { .. } => 0,
            })
            .sum()
    }

    pub fn blocked(&self) -> Vec<&StoreReport> {
        self.stores
            .iter()
            .filter(|store| matches!(store.verdict, StoreVerdict::Blocked { .. }))
            .collect()
    }

    /// True when every candidate was either removed or refused for a stated
    /// reason, and nothing else was found.
    pub fn is_clean(&self) -> bool {
        self.stores
            .iter()
            .all(|store| store.removed || matches!(store.verdict, StoreVerdict::Blocked { .. }))
    }
}

/// Every legacy store root under one state directory.
///
/// The candidates are exactly the paths the retired store was constructed
/// with: `<state>/projects/<project>/memory` and
/// `<state>/no-workspace/memory`.
pub fn candidates(state_root: &Path) -> Result<Vec<PathBuf>, PurgeError> {
    // A symlinked state root is not walked at all: the delete scope must be
    // the real directory the home resolution points at.
    match fs::symlink_metadata(state_root) {
        Ok(metadata) if metadata.file_type().is_symlink() => {
            return Err(PurgeError::Refused {
                path: state_root.to_path_buf(),
                reason: "the state root is a symlink".into(),
            });
        }
        _ => {}
    }
    let mut found = Vec::new();
    let no_workspace = state_root.join("no-workspace").join("memory");
    if is_real_dir(&no_workspace) {
        found.push(no_workspace);
    }
    let projects = state_root.join("projects");
    let entries = match fs::read_dir(&projects) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(found),
        Err(error) => return Err(PurgeError::io(&projects, error)),
    };
    let mut project_dirs = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|error| PurgeError::io(&projects, error))?;
        if is_real_dir(&entry.path()) {
            project_dirs.push(entry.path());
        }
    }
    project_dirs.sort();
    for project in project_dirs {
        let memory = project.join("memory");
        if is_real_dir(&memory) {
            found.push(memory);
        }
    }
    Ok(found)
}

fn is_real_dir(path: &Path) -> bool {
    match fs::symlink_metadata(path) {
        Ok(metadata) => metadata.file_type().is_dir(),
        Err(_) => false,
    }
}

/// Inspect one candidate without changing anything.
pub fn inspect(root: &Path, state_root: &Path) -> Result<StoreVerdict, PurgeError> {
    // The candidate must be exactly where the legacy store lived, and its
    // canonical form must stay inside the canonical state root.
    let canonical_root = root
        .canonicalize()
        .map_err(|error| PurgeError::io(root, error))?;
    let canonical_state = state_root
        .canonicalize()
        .map_err(|error| PurgeError::io(state_root, error))?;
    if !canonical_root.starts_with(&canonical_state) {
        return Ok(StoreVerdict::Blocked {
            reason: format!(
                "{} is outside the state root {}",
                canonical_root.display(),
                canonical_state.display()
            ),
        });
    }
    let name = root.file_name().and_then(|name| name.to_str());
    if name != Some("memory") {
        return Ok(StoreVerdict::Blocked {
            reason: "not a legacy memory root (its name is not `memory`)".into(),
        });
    }
    let parent_name = root
        .parent()
        .and_then(|parent| parent.file_name())
        .and_then(|name| name.to_str());
    let in_projects = root
        .parent()
        .and_then(|parent| parent.parent())
        .and_then(|base| base.file_name())
        .and_then(|name| name.to_str())
        == Some("projects");
    if !in_projects && parent_name != Some("no-workspace") {
        return Ok(StoreVerdict::Blocked {
            reason: "not a project memory root".into(),
        });
    }

    let entries = fs::read_dir(root).map_err(|error| PurgeError::io(root, error))?;
    let mut files = 0usize;
    let mut bytes = 0u64;
    let mut unknown = Vec::new();
    let allowed_dirs: BTreeSet<&str> = LEGACY_DIRS.into_iter().collect();
    for entry in entries {
        let entry = entry.map_err(|error| PurgeError::io(root, error))?;
        let path = entry.path();
        let file_type = entry
            .file_type()
            .map_err(|error| PurgeError::io(&path, error))?;
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if file_type.is_symlink() {
            unknown.push(format!("symlink {name}"));
            continue;
        }
        if file_type.is_dir() {
            if !allowed_dirs.contains(name.as_ref()) {
                unknown.push(format!("directory {name}"));
                continue;
            }
            let inner = fs::read_dir(&path).map_err(|error| PurgeError::io(&path, error))?;
            for item in inner {
                let item = item.map_err(|error| PurgeError::io(&path, error))?;
                let item_path = item.path();
                let item_type = item
                    .file_type()
                    .map_err(|error| PurgeError::io(&item_path, error))?;
                let item_name = item.file_name().to_string_lossy().to_string();
                let json = Path::new(&item_name)
                    .extension()
                    .is_some_and(|extension| extension == "json");
                if !item_type.is_file() || !json {
                    // Every record the JSON store wrote was a `.json` file in a
                    // known directory. Anything else is not ours to delete.
                    unknown.push(format!("{name}/{item_name}"));
                    continue;
                }
                let metadata = item
                    .metadata()
                    .map_err(|error| PurgeError::io(&item_path, error))?;
                files += 1;
                bytes += metadata.len();
            }
            continue;
        }
        if file_type.is_file() && LEGACY_FILES.contains(&name.as_ref()) {
            let metadata = entry
                .metadata()
                .map_err(|error| PurgeError::io(&path, error))?;
            files += 1;
            bytes += metadata.len();
            continue;
        }
        unknown.push(format!("file {name}"));
    }
    if !unknown.is_empty() {
        return Ok(StoreVerdict::Blocked {
            reason: format!(
                "unknown contents, refusing to delete: {}",
                unknown.join(", ")
            ),
        });
    }
    Ok(StoreVerdict::LegacyStore { files, bytes })
}

/// Scan every candidate store. `apply = false` is a metadata-only dry run.
pub fn purge(state_root: &Path, apply: bool) -> Result<PurgeReport, PurgeError> {
    let mut report = PurgeReport::default();
    for root in candidates(state_root)? {
        let verdict = inspect(&root, state_root)?;
        let mut removed = false;
        if apply && matches!(verdict, StoreVerdict::LegacyStore { .. }) {
            // The verdict proved every entry is a legacy artifact, so removing
            // the whole tree cannot take anything this tool does not own.
            fs::remove_dir_all(&root).map_err(|error| PurgeError::io(&root, error))?;
            removed = !root.exists();
        }
        report.stores.push(StoreReport {
            path: root,
            verdict,
            removed,
        });
    }
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn state_root(dir: &std::path::Path) -> PathBuf {
        dir.join("leveler-state")
    }

    fn legacy_store(state_root: &std::path::Path, project: &str) -> PathBuf {
        let root = state_root.join("projects").join(project).join("memory");
        for sub in LEGACY_DIRS {
            fs::create_dir_all(root.join(sub)).unwrap();
        }
        fs::write(
            root.join("active").join("mem-1.json"),
            br#"{"id":"mem-1","title":"t","body":"b"}"#,
        )
        .unwrap();
        fs::write(root.join("pending_history.jsonl"), b"{}\n").unwrap();
        fs::write(root.join(".lifecycle.lock"), b"").unwrap();
        root
    }

    #[test]
    fn a_legacy_store_is_removed_and_neighbours_survive() {
        let dir = tempfile::tempdir().unwrap();
        let state = state_root(dir.path());
        let store = legacy_store(&state, "-repo-a");
        let project = store.parent().unwrap();
        // Neighbours that are NOT the legacy memory store.
        fs::write(project.join("sessions.db"), b"sqlite").unwrap();
        fs::write(project.join("config.json"), b"{}").unwrap();
        let v2_state = project.join("memory-v2").join("project");
        fs::create_dir_all(&v2_state).unwrap();
        fs::write(v2_state.join("index.sqlite"), b"index").unwrap();
        // Markdown current records live in the repository, never here, but put
        // one at the same depth to prove the tool only matches `memory/`.
        let markdown = project.join(".leveler-memory");
        fs::create_dir_all(&markdown).unwrap();
        fs::write(markdown.join("record.md"), b"---\n---\nbody").unwrap();

        let dry = purge(&state, false).unwrap();
        assert_eq!(dry.stores.len(), 1);
        assert_eq!(dry.removed_stores(), 0, "a dry run writes nothing");
        assert!(store.exists());
        assert_eq!(dry.removed_files(), 0);

        let report = purge(&state, true).unwrap();
        assert_eq!(report.removed_stores(), 1);
        assert_eq!(report.removed_files(), 3);
        assert!(!store.exists(), "the legacy store is gone");
        assert!(project.join("sessions.db").exists());
        assert!(project.join("config.json").exists());
        assert!(v2_state.join("index.sqlite").exists());
        assert!(markdown.join("record.md").exists());

        // Idempotent: a second run finds nothing and changes nothing.
        let again = purge(&state, true).unwrap();
        assert!(again.stores.is_empty());
        assert!(again.is_clean());
    }

    #[test]
    fn a_missing_state_root_is_not_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("never-existed");
        assert!(candidates(&missing).unwrap().is_empty());
        let report = purge(&missing, true).unwrap();
        assert!(report.stores.is_empty());
    }

    #[test]
    fn an_unknown_entry_blocks_the_whole_store() {
        let dir = tempfile::tempdir().unwrap();
        let state = state_root(dir.path());
        let store = legacy_store(&state, "-repo-a");
        fs::create_dir_all(store.join("exports")).unwrap();
        let report = purge(&state, true).unwrap();
        assert_eq!(report.stores.len(), 1);
        assert_eq!(report.removed_stores(), 0);
        assert!(store.exists(), "an unrecognized store is never deleted");
        assert_eq!(report.blocked().len(), 1);
        let StoreVerdict::Blocked { reason } = &report.blocked()[0].verdict else {
            panic!("expected a block");
        };
        assert!(reason.contains("exports"), "{reason}");
    }

    #[test]
    fn a_case_variant_namespace_is_not_treated_as_a_legacy_store() {
        // On a case-insensitive filesystem `ACTIVE` IS `active`, so there is no
        // second namespace to misread. Only a case-sensitive filesystem can
        // prove the distinction.
        let probe = tempfile::tempdir().unwrap();
        fs::create_dir_all(probe.path().join("active")).unwrap();
        if probe.path().join("ACTIVE").exists() {
            return;
        }
        // The store created `active/archive/pending/suppress`. An `ACTIVE/`
        // directory is a different thing on a case-sensitive filesystem and
        // must not be guessed at.
        let dir = tempfile::tempdir().unwrap();
        let state = state_root(dir.path());
        let store = legacy_store(&state, "-repo-a");
        fs::create_dir_all(store.join("ACTIVE")).unwrap();
        fs::write(store.join("ACTIVE").join("x.json"), b"{}").unwrap();
        let report = purge(&state, true).unwrap();
        assert_eq!(report.removed_stores(), 0);
        assert!(store.exists());
        assert_eq!(report.blocked().len(), 1);
    }

    #[test]
    fn a_markdown_record_inside_the_store_blocks_deletion() {
        let dir = tempfile::tempdir().unwrap();
        let state = state_root(dir.path());
        let store = legacy_store(&state, "-repo-a");
        fs::write(store.join("active").join("record.md"), b"---\n---\nbody").unwrap();
        let report = purge(&state, true).unwrap();
        assert_eq!(report.removed_stores(), 0, "a .md file is not legacy data");
        assert!(store.exists());
    }

    #[cfg(unix)]
    #[test]
    fn a_symlink_never_extends_the_delete_scope() {
        let dir = tempfile::tempdir().unwrap();
        let state = state_root(dir.path());
        let store = legacy_store(&state, "-repo-a");
        let outside = dir.path().join("outside");
        fs::create_dir_all(&outside).unwrap();
        std::os::unix::fs::symlink(&outside, store.join("active").join("link")).unwrap();
        let report = purge(&state, true).unwrap();
        assert_eq!(report.removed_stores(), 0);
        assert!(outside.exists(), "the symlink target was touched");
        assert!(store.exists());

        // A symlinked state root is refused outright, not walked.
        let symlinked_root = dir.path().join("linked-state");
        std::os::unix::fs::symlink(&state, &symlinked_root).unwrap();
        assert!(matches!(
            candidates(&symlinked_root),
            Err(PurgeError::Refused { .. })
        ));
    }

    #[cfg(unix)]
    #[test]
    fn an_unreadable_store_reports_the_failure() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let state = state_root(dir.path());
        let store = legacy_store(&state, "-repo-a");
        let active = store.join("active");
        fs::set_permissions(&active, fs::Permissions::from_mode(0o0)).unwrap();
        let result = purge(&state, true);
        fs::set_permissions(&active, fs::Permissions::from_mode(0o700)).unwrap();
        match result {
            Err(error) => {
                assert!(error.to_string().contains("cannot read"), "{error}");
            }
            Ok(report) => {
                // A privileged user can still read it: then it must be a clean
                // removal, never a silent partial one.
                assert_eq!(report.removed_stores(), 1, "{report:?}");
                assert!(!store.exists());
            }
        }
    }
}
