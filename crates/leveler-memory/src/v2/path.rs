//! Scope roots, path safety, Git boundaries and atomic file primitives.

use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use super::record::MemoryScope;

pub const LOCK_TIMEOUT: Duration = Duration::from_secs(5);
const LOCK_BACKOFF: Duration = Duration::from_millis(10);

#[derive(Debug, thiserror::Error)]
pub enum PathError {
    #[error("scope root path is unavailable: {0}")]
    ScopeUnavailable(String),
    #[error("current_root and state_root must not contain each other")]
    RootsNested,
    #[error("state_root must not be inside the repository")]
    StateInsideRepo,
    #[error("managed path is a symlink: {0}")]
    Symlink(PathBuf),
    #[error("managed path escapes its root: {0}")]
    Escape(PathBuf),
    #[error("memory lock timeout")]
    LockTimeout,
    #[error("local memory is not ignored by Git: {0}")]
    LocalNotIgnored(PathBuf),
    #[error("Git ignore file is missing required lines: {0}")]
    GitIgnoreConflict(PathBuf),
    #[error("I/O error at {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
}

impl PathError {
    fn io(path: impl Into<PathBuf>, source: std::io::Error) -> Self {
        Self::Io {
            path: path.into(),
            source,
        }
    }
}

#[derive(Debug, Clone)]
pub struct ScopeRoots {
    pub scope: MemoryScope,
    pub current: PathBuf,
    pub state: PathBuf,
    /// Repository root for Git boundary checks. Required for local scope.
    pub repo_root: Option<PathBuf>,
}

impl ScopeRoots {
    pub fn new(scope: MemoryScope, current: impl Into<PathBuf>, state: impl Into<PathBuf>) -> Self {
        Self {
            scope,
            current: current.into(),
            state: state.into(),
            repo_root: None,
        }
    }

    pub fn with_repo_root(mut self, repo_root: impl Into<PathBuf>) -> Self {
        self.repo_root = Some(repo_root.into());
        self
    }
}

#[derive(Debug, Clone)]
pub struct StorePaths {
    pub scope: MemoryScope,
    pub current_root: PathBuf,
    pub state_root: PathBuf,
    pub stage_dir: PathBuf,
    pub history_dir: PathBuf,
    pub pending_dir: PathBuf,
    pub attic_dir: PathBuf,
    pub tombstones_dir: PathBuf,
    pub receipts_dir: PathBuf,
    pub intents_dir: PathBuf,
    pub journal_dir: PathBuf,
    pub catalog_dir: PathBuf,
    pub index_dir: PathBuf,
    pub lock_path: PathBuf,
    pub suppressed_path: PathBuf,
    pub pending_history_path: PathBuf,
    pub repo_root: Option<PathBuf>,
}

impl StorePaths {
    pub fn prepare(roots: &ScopeRoots) -> Result<Self, PathError> {
        if roots.current.as_os_str().is_empty() || roots.state.as_os_str().is_empty() {
            return Err(PathError::ScopeUnavailable(
                "current_root and state_root are required".into(),
            ));
        }
        create_dir(&roots.current)?;
        create_dir(&roots.state)?;
        reject_symlink(&roots.current)?;
        reject_symlink(&roots.state)?;
        let current = fs::canonicalize(&roots.current)
            .map_err(|error| PathError::io(&roots.current, error))?;
        let state =
            fs::canonicalize(&roots.state).map_err(|error| PathError::io(&roots.state, error))?;
        let repo = match &roots.repo_root {
            Some(repo_root) => {
                Some(fs::canonicalize(repo_root).map_err(|error| PathError::io(repo_root, error))?)
            }
            None => None,
        };
        check_layout(roots.scope, &current, &state, repo.as_deref())?;

        let paths = Self::layout(roots.scope, current, state, roots.repo_root.clone());
        for dir in [
            &paths.history_dir,
            &paths.pending_dir,
            &paths.attic_dir,
            &paths.tombstones_dir,
            &paths.receipts_dir,
            &paths.intents_dir,
            &paths.journal_dir,
            &paths.catalog_dir,
            &paths.index_dir,
        ] {
            create_dir(dir)?;
            reject_symlink(dir)?;
            secure_dir(dir);
        }
        secure_dir(&paths.state_root);
        secure_dir(&paths.current_root);
        paths.ensure_git_boundary()?;
        create_dir(&paths.stage_dir)?;
        reject_symlink(&paths.stage_dir)?;
        secure_dir(&paths.stage_dir);
        Ok(paths)
    }

    /// Resolve the same layout without creating, securing, or validating the
    /// roots. Read-only diagnostics use this: a store that was never
    /// initialized is a reportable state, not an error to create its way out
    /// of, and inspection must not recover or repair anything.
    pub fn inspect(roots: &ScopeRoots) -> Result<Self, PathError> {
        if roots.current.as_os_str().is_empty() || roots.state.as_os_str().is_empty() {
            return Err(PathError::ScopeUnavailable(
                "current_root and state_root are required".into(),
            ));
        }
        let current = absolute(&roots.current)?;
        let state = absolute(&roots.state)?;
        if current == state || is_inside(&current, &state) || is_inside(&state, &current) {
            return Err(PathError::RootsNested);
        }
        Ok(Self::layout(
            roots.scope,
            current,
            state,
            roots.repo_root.clone(),
        ))
    }

    /// Validate the roots a store would occupy without creating anything.
    ///
    /// A caller that finds no store yet still has to answer for the layout: a
    /// configuration that can never be opened must be reported, not read as
    /// an empty store. Every structural rule [`Self::prepare`] can answer
    /// before it creates a directory is answered here. The checks that need
    /// the roots on disk — symlink rejection and the local-scope Git ignore
    /// probe — stay with the write that creates them.
    pub fn validate(roots: &ScopeRoots) -> Result<(), PathError> {
        if roots.current.as_os_str().is_empty() || roots.state.as_os_str().is_empty() {
            return Err(PathError::ScopeUnavailable(
                "current_root and state_root are required".into(),
            ));
        }
        let current = absolute(&roots.current)?;
        let state = absolute(&roots.state)?;
        let repo = match &roots.repo_root {
            Some(repo_root) => Some(absolute(repo_root)?),
            None => None,
        };
        check_layout(roots.scope, &current, &state, repo.as_deref())
    }

    fn layout(
        scope: MemoryScope,
        current: PathBuf,
        state: PathBuf,
        repo_root: Option<PathBuf>,
    ) -> Self {
        let stage_dir = current.join(".stage");
        Self {
            scope,
            current_root: current,
            state_root: state.clone(),
            stage_dir,
            history_dir: state.join("history"),
            pending_dir: state.join("pending"),
            attic_dir: state.join("attic"),
            tombstones_dir: state.join("tombstones"),
            receipts_dir: state.join("receipts"),
            intents_dir: state.join("intents"),
            journal_dir: state.join("journal"),
            catalog_dir: state.join("catalog"),
            index_dir: state.join("index"),
            lock_path: state.join("lock"),
            suppressed_path: state.join("suppressed.jsonl"),
            pending_history_path: state.join("pending_history.jsonl"),
            repo_root,
        }
    }

    fn ensure_git_boundary(&self) -> Result<(), PathError> {
        match self.scope {
            MemoryScope::Local => {
                let repo = self.repo_root.as_ref().ok_or_else(|| {
                    PathError::ScopeUnavailable("local scope requires repo_root".into())
                })?;
                let repo = fs::canonicalize(repo).map_err(|error| PathError::io(repo, error))?;
                if self.current_root == repo {
                    return Err(PathError::ScopeUnavailable(
                        "local root must not be the repository root".into(),
                    ));
                }
                ensure_dedicated_ignore(&self.current_root, &["*", "!.gitignore"])?;
                if is_inside(&self.current_root, &repo) {
                    let probe = self.current_root.join("mem_probe.md");
                    match git_ignored(&repo, &probe) {
                        Some(true) => {}
                        Some(false) => {
                            return Err(PathError::LocalNotIgnored(self.current_root.clone()));
                        }
                        None => {
                            return Err(PathError::ScopeUnavailable(
                                "cannot verify local memory is ignored by Git".into(),
                            ));
                        }
                    }
                }
            }
            MemoryScope::Project => {
                ensure_dedicated_ignore(&self.current_root, &[".stage/", ".tmp-*"])?;
            }
            MemoryScope::Global => {}
        }
        Ok(())
    }

    pub fn current_file(&self, id: &super::record::MemoryId) -> PathBuf {
        self.current_root.join(id.file_name())
    }

    pub fn history_dir_for(&self, id: &super::record::MemoryId) -> PathBuf {
        self.history_dir.join(id.as_str())
    }

    pub fn history_file(&self, id: &super::record::MemoryId, revision: u64) -> PathBuf {
        self.history_dir_for(id).join(format!("{revision:04}.md"))
    }

    pub fn pending_file(&self, id: &super::record::MemoryId) -> PathBuf {
        self.pending_dir.join(id.file_name())
    }

    pub fn attic_file(&self, id: &super::record::MemoryId) -> PathBuf {
        self.attic_dir.join(format!("{}.json", id.as_str()))
    }

    pub fn tombstone_file(&self, id: &super::record::MemoryId) -> PathBuf {
        self.tombstones_dir.join(format!("{}.json", id.as_str()))
    }

    pub fn receipt_file(&self, id: &super::record::MemoryId) -> PathBuf {
        self.receipts_dir.join(format!("{}.json", id.as_str()))
    }

    pub fn intent_file(&self, op: &super::record::OpId) -> PathBuf {
        self.intents_dir.join(format!(
            "{}.json",
            super::frontmatter::sha256_hex_text(op.as_str())
        ))
    }

    pub fn journal_file(&self, op: &super::record::OpId) -> PathBuf {
        self.journal_dir.join(format!(
            "{}.json",
            super::frontmatter::sha256_hex_text(op.as_str())
        ))
    }
}

fn create_dir(path: &Path) -> Result<(), PathError> {
    fs::create_dir_all(path).map_err(|error| PathError::io(path, error))
}

pub fn reject_symlink(path: &Path) -> Result<(), PathError> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() => Err(PathError::Symlink(path.into())),
        Ok(_) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(PathError::io(path, error)),
    }
}

pub fn read_bytes(path: &Path) -> Result<Vec<u8>, PathError> {
    reject_symlink(path)?;
    let mut file = File::open(path).map_err(|error| PathError::io(path, error))?;
    let metadata = file
        .metadata()
        .map_err(|error| PathError::io(path, error))?;
    if !metadata.is_file() {
        return Err(PathError::ScopeUnavailable(format!(
            "managed path is not a file: {}",
            path.display()
        )));
    }
    let mut bytes = Vec::with_capacity(metadata.len() as usize + 1);
    file.read_to_end(&mut bytes)
        .map_err(|error| PathError::io(path, error))?;
    Ok(bytes)
}

pub fn read_json<T: serde::de::DeserializeOwned>(path: &Path) -> Result<Option<T>, PathError> {
    match read_bytes(path) {
        Ok(bytes) => serde_json::from_slice(&bytes)
            .map(Some)
            .map_err(|error| PathError::ScopeUnavailable(format!("{}: {error}", path.display()))),
        Err(PathError::Io { source, .. }) if source.kind() == std::io::ErrorKind::NotFound => {
            Ok(None)
        }
        Err(error) => Err(error),
    }
}

pub fn atomic_write(path: &Path, bytes: &[u8]) -> Result<(), PathError> {
    let parent = path
        .parent()
        .ok_or_else(|| PathError::ScopeUnavailable("managed file has no parent".into()))?;
    create_dir(parent)?;
    reject_symlink(path)?;
    let sequence = temp_sequence();
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("memory");
    let temp = parent.join(format!(".tmp-{}-{sequence}-{name}", std::process::id()));
    reject_symlink(&temp)?;
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    set_mode(&mut options, 0o600);
    let mut file = options
        .open(&temp)
        .map_err(|error| PathError::io(&temp, error))?;
    file.write_all(bytes)
        .map_err(|error| PathError::io(&temp, error))?;
    file.sync_all()
        .map_err(|error| PathError::io(&temp, error))?;
    drop(file);
    fs::rename(&temp, path).map_err(|error| PathError::io(path, error))?;
    fsync_dir(parent)?;
    Ok(())
}

pub fn write_new(path: &Path, bytes: &[u8], mode: u32) -> Result<(), PathError> {
    reject_symlink(path)?;
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    set_mode(&mut options, mode);
    let mut file = options
        .open(path)
        .map_err(|error| PathError::io(path, error))?;
    file.write_all(bytes)
        .map_err(|error| PathError::io(path, error))?;
    file.sync_all()
        .map_err(|error| PathError::io(path, error))?;
    Ok(())
}

pub fn remove_if_exists(path: &Path) -> Result<bool, PathError> {
    reject_symlink(path)?;
    match fs::remove_file(path) {
        Ok(()) => Ok(true),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(PathError::io(path, error)),
    }
}

pub fn remove_dir_if_exists(path: &Path) -> Result<bool, PathError> {
    reject_symlink(path)?;
    match fs::remove_dir_all(path) {
        Ok(()) => Ok(true),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(PathError::io(path, error)),
    }
}

pub fn append_line(path: &Path, line: &str) -> Result<(), PathError> {
    reject_symlink(path)?;
    let mut options = OpenOptions::new();
    options.write(true).create(true).append(true);
    set_mode(&mut options, 0o600);
    let mut file = options
        .open(path)
        .map_err(|error| PathError::io(path, error))?;
    file.write_all(line.as_bytes())
        .map_err(|error| PathError::io(path, error))?;
    file.write_all(b"\n")
        .map_err(|error| PathError::io(path, error))?;
    file.sync_all()
        .map_err(|error| PathError::io(path, error))?;
    Ok(())
}

pub fn list_markdown(dir: &Path) -> Result<Vec<PathBuf>, PathError> {
    let mut paths = Vec::new();
    let entries = match fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(paths),
        Err(error) => return Err(PathError::io(dir, error)),
    };
    for entry in entries {
        let entry = entry.map_err(|error| PathError::io(dir, error))?;
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if name.starts_with('.') || !name.ends_with(".md") {
            continue;
        }
        let file_type = entry
            .file_type()
            .map_err(|error| PathError::io(entry.path(), error))?;
        if file_type.is_file() {
            paths.push(entry.path());
        }
    }
    paths.sort();
    Ok(paths)
}

/// Every regular file directly under `dir`, including dot files. Used by
/// read-only diagnostics to report transaction temporaries that a normal
/// recovery would clean up.
pub fn list_regular_files(dir: &Path) -> Result<Vec<PathBuf>, PathError> {
    let mut paths = Vec::new();
    let entries = match fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(paths),
        Err(error) => return Err(PathError::io(dir, error)),
    };
    for entry in entries {
        let entry = entry.map_err(|error| PathError::io(dir, error))?;
        let file_type = entry
            .file_type()
            .map_err(|error| PathError::io(entry.path(), error))?;
        if file_type.is_file() {
            paths.push(entry.path());
        }
    }
    paths.sort();
    Ok(paths)
}

pub fn list_json(dir: &Path) -> Result<Vec<PathBuf>, PathError> {
    let mut paths = Vec::new();
    let entries = match fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(paths),
        Err(error) => return Err(PathError::io(dir, error)),
    };
    for entry in entries {
        let entry = entry.map_err(|error| PathError::io(dir, error))?;
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if name.starts_with('.') || !name.ends_with(".json") {
            continue;
        }
        let file_type = entry
            .file_type()
            .map_err(|error| PathError::io(entry.path(), error))?;
        if file_type.is_file() {
            paths.push(entry.path());
        }
    }
    paths.sort();
    Ok(paths)
}

pub fn fsync_dir(path: &Path) -> Result<(), PathError> {
    #[cfg(unix)]
    {
        let dir = File::open(path).map_err(|error| PathError::io(path, error))?;
        dir.sync_all().map_err(|error| PathError::io(path, error))?;
    }
    // Non-Unix platforms have no supported directory fsync. The file rename is
    // still atomic, but directory-entry durability is not claimed there.
    let _ = path;
    Ok(())
}

pub struct ScopeLock {
    file: File,
}

impl ScopeLock {
    pub fn acquire(path: &Path, exclusive: bool) -> Result<Self, PathError> {
        reject_symlink(path)?;
        let mut options = OpenOptions::new();
        options.read(true).write(true).create(true);
        set_mode(&mut options, 0o600);
        let file = options
            .open(path)
            .map_err(|error| PathError::io(path, error))?;
        let deadline = Instant::now() + LOCK_TIMEOUT;
        loop {
            let result = if exclusive {
                fs2::FileExt::try_lock_exclusive(&file)
            } else {
                fs2::FileExt::try_lock_shared(&file)
            };
            match result {
                Ok(()) => return Ok(Self { file }),
                Err(_) if Instant::now() >= deadline => return Err(PathError::LockTimeout),
                Err(_) => std::thread::sleep(LOCK_BACKOFF),
            }
        }
    }
}

impl Drop for ScopeLock {
    fn drop(&mut self) {
        let _ = fs2::FileExt::unlock(&self.file);
    }
}

fn ensure_dedicated_ignore(dir: &Path, required: &[&str]) -> Result<(), PathError> {
    let path = dir.join(".gitignore");
    let exists = match fs::symlink_metadata(&path) {
        Ok(_) => true,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => false,
        Err(error) => return Err(PathError::io(&path, error)),
    };
    if !exists {
        let mut entries = fs::read_dir(dir).map_err(|error| PathError::io(dir, error))?;
        if entries.next().is_some() {
            // Never silently ignore an existing directory that may hold
            // unrelated content. The caller must point at a dedicated root.
            return Err(PathError::GitIgnoreConflict(path));
        }
    }
    ensure_ignore_lines(&path, required)
}

fn ensure_ignore_lines(path: &Path, required: &[&str]) -> Result<(), PathError> {
    reject_symlink(path)?;
    let content = match fs::read_to_string(path) {
        Ok(content) => content,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            let mut content = String::new();
            for line in required {
                content.push_str(line);
                content.push('\n');
            }
            write_new(path, content.as_bytes(), 0o644)?;
            return Ok(());
        }
        Err(error) => return Err(PathError::io(path, error)),
    };
    let lines: Vec<&str> = content.lines().map(str::trim).collect();
    if required.iter().all(|required| lines.contains(required)) {
        Ok(())
    } else {
        Err(PathError::GitIgnoreConflict(path.into()))
    }
}

fn git_ignored(repo_root: &Path, path: &Path) -> Option<bool> {
    let output = Command::new("git")
        .arg("-C")
        .arg(repo_root)
        .arg("check-ignore")
        .arg("-q")
        .arg(path)
        .output()
        .ok()?;
    match output.status.code() {
        Some(0) => Some(true),
        Some(1) => Some(false),
        _ => None,
    }
}

pub fn is_inside(child: &Path, parent: &Path) -> bool {
    child != parent && child.starts_with(parent)
}

/// Best-effort absolute path for inspection. A root that does not exist yet
/// keeps its spelling instead of being created just to be inspected, but its
/// deepest existing ancestor is resolved: two spellings of the same directory
/// must compare equal, or a boundary check would answer about the spelling
/// rather than the path.
fn absolute(path: &Path) -> Result<PathBuf, PathError> {
    let mut candidate = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .map_err(|error| PathError::io(path, error))?
            .join(path)
    };
    let mut tail: Vec<std::ffi::OsString> = Vec::new();
    loop {
        match fs::canonicalize(&candidate) {
            Ok(mut resolved) => {
                for part in tail.iter().rev() {
                    resolved.push(part);
                }
                return Ok(resolved);
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                match (candidate.file_name(), candidate.parent()) {
                    (Some(name), Some(parent)) => {
                        tail.push(name.to_owned());
                        candidate = parent.to_path_buf();
                    }
                    _ => return Err(PathError::io(&candidate, error)),
                }
            }
            Err(error) => return Err(PathError::io(&candidate, error)),
        }
    }
}

/// Structural rules a store layout satisfies before any directory is created.
///
/// [`StorePaths::prepare`] runs it on canonical paths; [`StorePaths::validate`]
/// runs it on the paths a store would occupy. One owner, so a read that finds
/// no store still answers for the same layout an open would.
fn check_layout(
    scope: MemoryScope,
    current: &Path,
    state: &Path,
    repo_root: Option<&Path>,
) -> Result<(), PathError> {
    if current == state || is_inside(current, state) || is_inside(state, current) {
        return Err(PathError::RootsNested);
    }
    if let Some(repo_root) = repo_root
        && (is_inside(state, repo_root)
            || is_inside(state, &repo_root.join(".git"))
            || is_inside(current, &repo_root.join(".git")))
    {
        return Err(PathError::StateInsideRepo);
    }
    if scope == MemoryScope::Local && repo_root.is_none() {
        return Err(PathError::ScopeUnavailable(
            "local scope requires repo_root for Git isolation".into(),
        ));
    }
    Ok(())
}

fn temp_sequence() -> u64 {
    use std::sync::atomic::{AtomicU64, Ordering};
    static SEQUENCE: AtomicU64 = AtomicU64::new(0);
    SEQUENCE.fetch_add(1, Ordering::Relaxed)
}

fn set_mode(options: &mut OpenOptions, mode: u32) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(mode);
    }
    let _ = (options, mode);
}

fn secure_dir(path: &Path) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = fs::set_permissions(path, fs::Permissions::from_mode(0o700));
    }
    let _ = path;
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn nested_roots_are_rejected() {
        let dir = tempdir().unwrap();
        let current = dir.path().join("current");
        let state = current.join("state");
        let roots = ScopeRoots::new(MemoryScope::Project, &current, &state);
        assert!(matches!(
            StorePaths::prepare(&roots),
            Err(PathError::RootsNested)
        ));
    }

    #[cfg(unix)]
    #[test]
    fn symlinked_current_root_is_rejected() {
        let dir = tempdir().unwrap();
        let target = dir.path().join("target");
        fs::create_dir(&target).unwrap();
        let current = dir.path().join("current");
        std::os::unix::fs::symlink(&target, &current).unwrap();
        let state = dir.path().join("state");
        let roots = ScopeRoots::new(MemoryScope::Project, &current, &state);
        assert!(matches!(
            StorePaths::prepare(&roots),
            Err(PathError::Symlink(_))
        ));
    }
}
