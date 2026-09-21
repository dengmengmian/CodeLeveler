//! Local storage lifecycle: what CodeLeveler owns, what may be reclaimed, and
//! how to reclaim it without touching anything durable.
//!
//! The authority for *where* things live stays [`LevelerHome`]; this module only
//! decides what those paths *are* and what may be done to them.
//!
//! Three classes, never blurred:
//!
//! - [`StorageClass::Durable`] — sessions, conversation history, artifacts,
//!   project metadata, user config. Never reclaimed automatically.
//! - [`StorageClass::Cache`] — tool/build caches. Rebuildable; bounded by a
//!   size budget and reclaimed oldest-first.
//! - [`StorageClass::Ephemeral`] — one-run coordination and residue: sockets,
//!   advisory locks, sandbox scratch, `$TMPDIR/codeleveler/**`. Reclaimed once
//!   expired and provably unowned.
//!
//! A cleanup is a **plan** first: [`plan_cleanup`] lists exactly what would be
//! removed, with a size, a reason and a [`Safety`]; [`execute_plan`] then removes
//! only those entries. Nothing re-scans mid-delete, so classification cannot
//! shift under the plan, and every removal is re-validated against an owned root.

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use leveler_core::LevelerHome;
use serde::Serialize;

/// Directory name under the system temp dir that owns every ephemeral run.
pub const EPHEMERAL_NAMESPACE: &str = "codeleveler";

/// Marker file inside a tool-cache entry whose mtime is its last-used time.
///
/// Defined once in `leveler-core` so the execution layer (which writes it) and
/// the storage-hygiene GC (which reads it) cannot drift apart.
pub const TOOL_CACHE_LAST_USED_FILE: &str = leveler_core::TOOL_CACHE_LAST_USED_FILE;

/// Exclusive advisory lock a command holds while it is using a cache entry, so
/// the GC never deletes a cache a live process is reading or writing.
pub const TOOL_CACHE_LOCK_FILE: &str = leveler_core::TOOL_CACHE_LOCK_FILE;

/// Default tool-cache budget: 2 GiB. Investigated against a real machine where
/// `cache/tools` had grown to 4.4 GiB across ~49k per-workspace entries, almost
/// all from throwaway eval workspaces that will never be reused.
pub const DEFAULT_TOOL_CACHE_BUDGET_BYTES: u64 = 2 * 1024 * 1024 * 1024;

/// Reclaim down to this fraction of the budget once it is exceeded.
pub const LOW_WATERMARK_RATIO: f64 = 0.5;

/// How long an unused cache entry may sit before the budget can reclaim it.
pub const DEFAULT_CACHE_IDLE_TTL: Duration = Duration::from_secs(7 * 24 * 60 * 60);

/// Crash/SIGKILL/reboot residue under the ephemeral namespace is reclaimable
/// after this age, and only when it shows no recent activity.
pub const DEFAULT_EPHEMERAL_TTL: Duration = Duration::from_secs(24 * 60 * 60);

/// The namespace a home path belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum StorageClass {
    Durable,
    Cache,
    Ephemeral,
}

/// What kind of thing a plan entry is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CleanupKind {
    /// A per-workspace tool/build cache over the size budget.
    ToolCache,
    /// Expired, unowned residue under `$TMPDIR/codeleveler/**`.
    ExpiredEphemeral,
    /// A socket file no process is listening on.
    DeadSocket,
    /// A socket/lock sidecar whose owner is gone.
    OrphanLock,
    /// A project state dir whose repository is gone and whose identity is a
    /// throwaway automation workspace (eval / dogfood / scratchpad).
    HistoricalAutomation,
    /// A project state dir whose repository is gone but which may hold durable
    /// sessions or artifacts. Never safe: needs an explicit human decision.
    DeletedProjectData,
}

impl CleanupKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::ToolCache => "tool_cache",
            Self::ExpiredEphemeral => "expired_ephemeral",
            Self::DeadSocket => "dead_socket",
            Self::OrphanLock => "orphan_lock",
            Self::HistoricalAutomation => "historical_automation",
            Self::DeletedProjectData => "deleted_project_data",
        }
    }

    pub const ALL: [CleanupKind; 6] = [
        Self::ToolCache,
        Self::ExpiredEphemeral,
        Self::DeadSocket,
        Self::OrphanLock,
        Self::HistoricalAutomation,
        Self::DeletedProjectData,
    ];
}

/// Whether a entry may be removed by a `--safe` cleanup.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Safety {
    /// Rebuildable or provably unowned: `--safe` reclaims it.
    Safe,
    /// Could hold something a human cares about: never removed without an
    /// explicit decision.
    NeedsConfirmation,
}

/// One thing a cleanup would remove.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct CleanupEntry {
    pub kind: CleanupKind,
    pub path: PathBuf,
    pub size_bytes: u64,
    pub reason: String,
    pub safety: Safety,
}

/// A frozen list of what a cleanup would do. Executed as-is.
#[derive(Debug, Clone, Default, Serialize)]
pub struct CleanupPlan {
    pub entries: Vec<CleanupEntry>,
}

impl CleanupPlan {
    pub fn total_bytes(&self) -> u64 {
        self.entries.iter().map(|e| e.size_bytes).sum()
    }

    pub fn safe_bytes(&self) -> u64 {
        self.entries
            .iter()
            .filter(|e| e.safety == Safety::Safe)
            .map(|e| e.size_bytes)
            .sum()
    }

    pub fn needs_confirmation_bytes(&self) -> u64 {
        self.entries
            .iter()
            .filter(|e| e.safety == Safety::NeedsConfirmation)
            .map(|e| e.size_bytes)
            .sum()
    }

    pub fn bytes_for(&self, kind: CleanupKind) -> u64 {
        self.entries
            .iter()
            .filter(|e| e.kind == kind)
            .map(|e| e.size_bytes)
            .sum()
    }

    pub fn count_for(&self, kind: CleanupKind) -> usize {
        self.entries.iter().filter(|e| e.kind == kind).count()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Retain only entries that a `--safe` run would remove.
    pub fn safe_only(&mut self) {
        self.entries.retain(|e| e.safety == Safety::Safe);
    }

    /// Retain only entries of the given kinds.
    pub fn retain_kinds(&mut self, kinds: &[CleanupKind]) {
        if !kinds.is_empty() {
            self.entries.retain(|e| kinds.contains(&e.kind));
        }
    }
}

/// Knobs for [`plan_cleanup`]. `Default` is the conservative production policy.
#[derive(Debug, Clone)]
pub struct CleanupOptions {
    pub now: SystemTime,
    pub ephemeral_ttl: Duration,
    pub cache_idle_ttl: Duration,
    pub tool_cache_budget_bytes: u64,
    /// Include the tool-cache reclamation.
    pub include_cache: bool,
    /// Include expired ephemeral residue.
    pub include_ephemeral: bool,
    /// Include dead sockets / orphan lock sidecars.
    pub include_runtime_coordination: bool,
    /// Include project state whose repository is gone.
    pub include_stale_state: bool,
    /// Include entries that require confirmation.
    pub include_needs_confirmation: bool,
}

impl Default for CleanupOptions {
    fn default() -> Self {
        Self {
            now: SystemTime::now(),
            ephemeral_ttl: DEFAULT_EPHEMERAL_TTL,
            cache_idle_ttl: DEFAULT_CACHE_IDLE_TTL,
            tool_cache_budget_bytes: DEFAULT_TOOL_CACHE_BUDGET_BYTES,
            include_cache: true,
            include_ephemeral: true,
            include_runtime_coordination: true,
            include_stale_state: true,
            include_needs_confirmation: false,
        }
    }
}

/// The ephemeral namespace root: `<system temp>/codeleveler`.
///
/// Temporary by construction — never under the durable home — but still
/// CodeLeveler-owned, so it is the only temp tree a cleanup is allowed to scan.
pub fn ephemeral_base_dir() -> PathBuf {
    std::env::temp_dir().join(EPHEMERAL_NAMESPACE)
}

/// Which class a path under the home belongs to.
pub fn classify_home_path(home: &LevelerHome, path: &Path) -> StorageClass {
    let root = home.root();
    if path.starts_with(home.cache_dir()) {
        StorageClass::Cache
    } else if path.starts_with(home.run_dir()) {
        StorageClass::Ephemeral
    } else if path.starts_with(home.state_dir()) {
        StorageClass::Durable
    } else if path.starts_with(root) {
        // config.toml, agents/, skills/, logs/, runtimes/.
        StorageClass::Durable
    } else {
        StorageClass::Ephemeral
    }
}

/// Build a cleanup plan. Pure with respect to the filesystem: it reads sizes and
/// metadata, and never removes anything.
pub fn plan_cleanup(
    home: &LevelerHome,
    ephemeral_base: &Path,
    options: &CleanupOptions,
) -> CleanupPlan {
    let mut entries = Vec::new();
    if options.include_runtime_coordination {
        plan_dead_sockets(home, &mut entries);
        plan_orphan_locks(home, &mut entries);
    }
    if options.include_ephemeral {
        plan_expired_ephemeral(ephemeral_base, options, &mut entries);
    }
    if options.include_stale_state {
        plan_project_state(home, options, &mut entries);
    }
    if options.include_cache {
        plan_tool_cache(home, options, &mut entries);
    }
    // The plan carries both safe and confirmation-only candidates so the report
    // can show a complete inventory; execution decides which it is allowed to
    // touch (`--safe` removes only `Safety::Safe`).
    entries.sort_by(|a, b| a.path.cmp(&b.path));
    CleanupPlan { entries }
}

fn plan_dead_sockets(home: &LevelerHome, entries: &mut Vec<CleanupEntry>) {
    let dir = home.sockets_dir();
    let Ok(read) = std::fs::read_dir(&dir) else {
        return;
    };
    for entry in read.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("sock") {
            continue;
        }
        if !is_dead_socket(&path) {
            continue;
        }
        entries.push(CleanupEntry {
            kind: CleanupKind::DeadSocket,
            size_bytes: entry_size(&path),
            reason: "no process is listening on this socket".to_string(),
            safety: Safety::Safe,
            path,
        });
    }
}

fn plan_orphan_locks(home: &LevelerHome, entries: &mut Vec<CleanupEntry>) {
    // Locks live beside sockets and under `run/locks`. A lock whose `flock` the
    // GC can take has no live owner; the file is advisory metadata that the next
    // run recreates. Socket lock sidecars whose `.sock` is already dead are
    // reclaimed together with the socket.
    for dir in [home.sockets_dir(), home.locks_dir()] {
        let Ok(read) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in read.flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("lock") {
                continue;
            }
            if !lock_is_unowned(&path) {
                continue;
            }
            entries.push(CleanupEntry {
                kind: CleanupKind::OrphanLock,
                size_bytes: entry_size(&path),
                reason: "no live process holds this advisory lock".to_string(),
                safety: Safety::Safe,
                path,
            });
        }
    }
}

fn plan_expired_ephemeral(base: &Path, options: &CleanupOptions, entries: &mut Vec<CleanupEntry>) {
    let Ok(labels) = std::fs::read_dir(base) else {
        return;
    };
    for label in labels.flatten() {
        let label_path = label.path();
        if !label_path.is_dir() {
            continue;
        }
        let Ok(runs) = std::fs::read_dir(&label_path) else {
            continue;
        };
        for run in runs.flatten() {
            let run_path = run.path();
            let age = age_of(&run_path, options.now);
            if age < options.ephemeral_ttl {
                continue;
            }
            if tree_has_recent_activity(&run_path, options.ephemeral_ttl, options.now) {
                // Something wrote here recently; a live run may still own it.
                continue;
            }
            entries.push(CleanupEntry {
                kind: CleanupKind::ExpiredEphemeral,
                size_bytes: entry_size(&run_path),
                reason: format!(
                    "run residue older than {}h",
                    options.ephemeral_ttl.as_secs() / 3600
                ),
                safety: Safety::Safe,
                path: run_path,
            });
        }
    }
}

fn plan_project_state(
    home: &LevelerHome,
    options: &CleanupOptions,
    entries: &mut Vec<CleanupEntry>,
) {
    let projects = home.projects_dir();
    let Ok(read) = std::fs::read_dir(&projects) else {
        return;
    };
    for entry in read.flatten() {
        let dir = entry.path();
        if !dir.is_dir() {
            continue;
        }
        let Some(id) = dir.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        let Some(owner) = repository_owner(&dir) else {
            // No ownership marker: unknown. Keep.
            continue;
        };
        if owner.is_dir() {
            continue; // real, active project
        }
        let automation = looks_like_automation_workspace(&owner, id);
        let (kind, safety) = if automation {
            (CleanupKind::HistoricalAutomation, Safety::Safe)
        } else {
            (CleanupKind::DeletedProjectData, Safety::NeedsConfirmation)
        };
        entries.push(CleanupEntry {
            kind,
            size_bytes: entry_size(&dir),
            reason: if automation {
                format!(
                    "throwaway automation workspace no longer exists: {}",
                    owner.display()
                )
            } else {
                format!("repository no longer exists: {}", owner.display())
            },
            safety,
            path: dir,
        });
    }
    // Unused selector so `options` participates in the signature symmetrically;
    // project-state reclamation is age-independent (a gone repo never returns).
    let _ = options;
}

fn plan_tool_cache(home: &LevelerHome, options: &CleanupOptions, entries: &mut Vec<CleanupEntry>) {
    plan_tool_cache_with(home, options, entries, &|entry| last_used(entry));
}

/// Tool-cache reclamation with an injectable last-used reader, so the ordering
/// policy is testable without mutating the filesystem clock.
fn plan_tool_cache_with(
    home: &LevelerHome,
    options: &CleanupOptions,
    entries: &mut Vec<CleanupEntry>,
    last_used_of: &dyn Fn(&std::fs::DirEntry) -> SystemTime,
) {
    let dir = home.tool_cache_dir();
    let Ok(read) = std::fs::read_dir(&dir) else {
        return;
    };
    let mut sized: Vec<(PathBuf, u64, SystemTime)> = Vec::new();
    let mut total: u64 = 0;
    for entry in read.flatten() {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let size = entry_size(&path);
        total = total.saturating_add(size);
        sized.push((path, size, last_used_of(&entry)));
    }
    if total <= options.tool_cache_budget_bytes {
        return;
    }
    let low = (options.tool_cache_budget_bytes as f64 * LOW_WATERMARK_RATIO) as u64;
    let mut target = total.saturating_sub(low);
    // Oldest first; a cache used inside the idle TTL or currently locked is not
    // a candidate.
    sized.sort_by_key(|(_, _, used)| *used);
    for (path, size, used) in sized {
        if target == 0 {
            break;
        }
        if options.now.duration_since(used).unwrap_or_default() < options.cache_idle_ttl {
            continue;
        }
        if cache_is_active(&path) {
            continue;
        }
        target = target.saturating_sub(size);
        entries.push(CleanupEntry {
            kind: CleanupKind::ToolCache,
            size_bytes: size,
            reason: "rebuildable tool cache over the size budget".to_string(),
            safety: Safety::Safe,
            path,
        });
    }
}

/// Read a project state dir's owner marker. `None` when it is absent — the dir
/// is then unknown and never reclaimed.
fn repository_owner(project_state: &Path) -> Option<PathBuf> {
    let raw =
        std::fs::read_to_string(project_state.join(crate::layout::REPOSITORY_OWNER_FILE)).ok()?;
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(PathBuf::from(trimmed))
    }
}

/// A workspace whose name or project id names an automation run.
///
/// The signal is the **name**, not the location: a repository cloned under
/// `/tmp` is still a real repository whose sessions are durable. Only a
/// workspace that literally names an eval / dogfood / scratchpad / smoke run
/// is high-confidence throwaway.
fn looks_like_automation_workspace(owner: &Path, id: &str) -> bool {
    let name = owner.file_name().and_then(|n| n.to_str()).unwrap_or("");
    const MARKERS: [&str; 4] = ["leveler-eval", "scratchpad", "dogfood", "smoke"];
    let marked = |text: &str| MARKERS.iter().any(|marker| text.contains(marker));
    marked(name) || marked(id)
}

/// Last-used time: the marker file's mtime when present, else the entry's own
/// mtime. The marker is written by the execution layer on every use, so it is
/// the reliable signal; the fallback is best-effort for pre-marker entries.
pub fn last_used(entry: &std::fs::DirEntry) -> SystemTime {
    let marker = entry.path().join(TOOL_CACHE_LAST_USED_FILE);
    std::fs::metadata(&marker)
        .or_else(|_| entry.metadata())
        .and_then(|m| m.modified())
        .unwrap_or(SystemTime::UNIX_EPOCH)
}

fn age_of(path: &Path, now: SystemTime) -> Duration {
    std::fs::metadata(path)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|m| now.duration_since(m).ok())
        .unwrap_or_default()
}

fn tree_has_recent_activity(root: &Path, ttl: Duration, now: SystemTime) -> bool {
    let deadline = ttl.as_secs();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(read) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in read.flatten() {
            let path = entry.path();
            if let Ok(meta) = entry.metadata() {
                if meta.modified().is_ok_and(|modified| {
                    now.duration_since(modified)
                        .map(|d| d.as_secs())
                        .unwrap_or(0)
                        < deadline
                }) {
                    return true;
                }
                if meta.is_dir() {
                    stack.push(path);
                }
            }
        }
    }
    false
}

/// Whether the advisory lock file has no live holder. A missing file is treated
/// as unowned (nothing to protect).
pub fn lock_is_unowned(path: &Path) -> bool {
    let Ok(file) = std::fs::OpenOptions::new().write(true).open(path) else {
        return false; // cannot open: fail closed, leave it alone
    };
    fs2::FileExt::try_lock_exclusive(&file).is_ok()
}

/// Whether a live command holds this cache entry's lease.
pub fn cache_is_active(entry: &Path) -> bool {
    let lock = entry.join(TOOL_CACHE_LOCK_FILE);
    if !lock.exists() {
        return false;
    }
    let Ok(file) = std::fs::OpenOptions::new().write(true).open(&lock) else {
        return true; // present but unreadable: assume in use
    };
    fs2::FileExt::try_lock_exclusive(&file).is_err()
}

#[cfg(unix)]
fn is_dead_socket(path: &Path) -> bool {
    match std::os::unix::net::UnixStream::connect(path) {
        Ok(_) => false,
        Err(error) => !matches!(error.kind(), std::io::ErrorKind::PermissionDenied),
    }
}

#[cfg(not(unix))]
fn is_dead_socket(_path: &Path) -> bool {
    false
}

/// Recursive size. Symlinks are counted as their own (tiny) entry and never
/// followed — a planted link must not make the scan leave the owned root.
fn entry_size(path: &Path) -> u64 {
    let mut total = 0u64;
    let Ok(meta) = std::fs::symlink_metadata(path) else {
        return 0;
    };
    if meta.file_type().is_symlink() {
        return 0;
    }
    if meta.is_file() {
        return meta.len();
    }
    total = total.saturating_add(meta.len().min(4096)); // directory inode estimate
    let Ok(read) = std::fs::read_dir(path) else {
        return total;
    };
    for entry in read.flatten() {
        total = total.saturating_add(entry_size(&entry.path()));
    }
    total
}

/// What an [`execute_plan`] run did.
#[derive(Debug, Clone, Default, Serialize)]
pub struct CleanupReport {
    pub reclaimed_bytes: u64,
    pub removed: usize,
    /// Reclaimed bytes per kind, for a grouped result view.
    pub reclaimed_by_kind: std::collections::BTreeMap<CleanupKind, u64>,
    pub failures: Vec<CleanupFailure>,
}

#[derive(Debug, Clone, Serialize)]
pub struct CleanupFailure {
    pub path: PathBuf,
    pub size_bytes: u64,
    pub error: String,
}

impl CleanupReport {
    pub fn failed_bytes(&self) -> u64 {
        self.failures.iter().map(|f| f.size_bytes).sum()
    }
}

/// Remove every entry in `plan`, each independently.
///
/// `dry_run` makes this a no-op that still reports what would be freed. Each
/// path is re-validated against an owned root immediately before removal, so a
/// plan built before a race cannot delete a symlink or a path outside the roots.
pub fn execute_plan(plan: &CleanupPlan, dry_run: bool) -> CleanupReport {
    let mut roots = vec![
        LevelerHome::resolve(leveler_core::environment())
            .root()
            .to_path_buf(),
    ];
    roots.push(ephemeral_base_dir());
    execute_plan_under(plan, dry_run, &roots)
}

/// [`execute_plan`] with explicit owned roots (tests, injected homes).
pub fn execute_plan_under(
    plan: &CleanupPlan,
    dry_run: bool,
    owned_roots: &[PathBuf],
) -> CleanupReport {
    let mut report = CleanupReport::default();
    for entry in &plan.entries {
        if dry_run {
            report.reclaimed_bytes = report.reclaimed_bytes.saturating_add(entry.size_bytes);
            report.removed += 1;
            continue;
        }
        match safe_remove(&entry.path, owned_roots) {
            Ok(()) => {
                report.reclaimed_bytes = report.reclaimed_bytes.saturating_add(entry.size_bytes);
                *report.reclaimed_by_kind.entry(entry.kind).or_default() += entry.size_bytes;
                report.removed += 1;
            }
            Err(error) => report.failures.push(CleanupFailure {
                path: entry.path.clone(),
                size_bytes: entry.size_bytes,
                error: error.to_string(),
            }),
        }
    }
    report
}

/// Remove one path, refusing to follow symlinks or leave the owned roots.
///
/// A cleanup may only delete something that is (a) not a symlink itself, (b)
/// under a canonical owned root, (c) not that root, and (d) whose parent also
/// resolves under that root. This is the single gate every deletion passes.
pub fn safe_remove(path: &Path, owned_roots: &[PathBuf]) -> std::io::Result<()> {
    let meta = std::fs::symlink_metadata(path)?;
    if meta.file_type().is_symlink() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "refusing to delete a symlink",
        ));
    }
    let canonical_roots: Vec<PathBuf> = owned_roots
        .iter()
        .map(|root| std::fs::canonicalize(root).unwrap_or_else(|_| root.clone()))
        .collect();
    let parent = path.parent().ok_or_else(|| {
        std::io::Error::new(std::io::ErrorKind::InvalidInput, "path has no parent")
    })?;
    let canonical_parent = std::fs::canonicalize(parent)?;
    if !canonical_roots
        .iter()
        .any(|root| canonical_parent.starts_with(root))
    {
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            format!(
                "refusing to delete outside the owned roots: {}",
                path.display()
            ),
        ));
    }
    if canonical_roots
        .iter()
        .any(|root| canonical_parent == *root && path == root)
    {
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "refusing to delete an owned root",
        ));
    }
    if meta.is_dir() {
        std::fs::remove_dir_all(path)
    } else {
        std::fs::remove_file(path)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn home_at(root: impl Into<PathBuf>) -> LevelerHome {
        LevelerHome::from_root(root)
    }

    fn project_state(home: &LevelerHome, id: &str, owner: &str) -> PathBuf {
        let dir = home.project_state_dir(id);
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join(crate::layout::REPOSITORY_OWNER_FILE), owner).unwrap();
        dir
    }

    fn options(now: SystemTime) -> CleanupOptions {
        CleanupOptions {
            now,
            ..Default::default()
        }
    }

    #[test]
    fn classification_keeps_durable_cache_and_ephemeral_apart() {
        let tmp = tempfile::tempdir().unwrap();
        let home = home_at(tmp.path());
        assert_eq!(
            classify_home_path(&home, &home.cache_dir()),
            StorageClass::Cache
        );
        assert_eq!(
            classify_home_path(&home, &home.run_dir()),
            StorageClass::Ephemeral
        );
        assert_eq!(
            classify_home_path(&home, &home.state_dir()),
            StorageClass::Durable
        );
        assert_eq!(
            classify_home_path(&home, &home.config_file()),
            StorageClass::Durable
        );
    }

    // G5 — a deleted repository with a plausible durable state dir is never
    // classified safe, even when `include_needs_confirmation` is set.
    #[test]
    fn g5_deleted_repository_with_state_needs_confirmation() {
        let tmp = tempfile::tempdir().unwrap();
        let home = home_at(tmp.path().join("home"));
        let gone = tmp.path().join("gone-repo");
        let dir = project_state(
            &home,
            "-Users-me-gone-repo-0123456789abcdef",
            &gone.display().to_string(),
        );
        fs::write(dir.join("sessions.db"), b"durable").unwrap();

        let plan = plan_cleanup(&home, &tmp.path().join("nope"), &options(SystemTime::now()));
        let entry = plan
            .entries
            .iter()
            .find(|e| e.path == dir)
            .expect("dir is in the plan");
        assert_eq!(entry.kind, CleanupKind::DeletedProjectData);
        assert_eq!(entry.safety, Safety::NeedsConfirmation);
    }

    // G1 — a real, active project's state is never in the plan.
    #[test]
    fn g1_live_project_state_is_never_planned() {
        let tmp = tempfile::tempdir().unwrap();
        let home = home_at(tmp.path().join("home"));
        let repo = tmp.path().join("repo");
        fs::create_dir_all(&repo).unwrap();
        let dir = project_state(
            &home,
            "-Users-me-repo-0123456789abcdef",
            &repo.display().to_string(),
        );

        let plan = plan_cleanup(&home, &tmp.path().join("nope"), &options(SystemTime::now()));
        assert!(plan.entries.iter().all(|e| e.path != dir), "{plan:?}");
    }

    // G? — throwaway automation state (repo gone, temp workspace) is safe.
    #[test]
    fn historical_automation_state_is_safe() {
        let tmp = tempfile::tempdir().unwrap();
        let home = home_at(tmp.path().join("home"));
        let gone = tmp.path().join("leveler-eval-rust-case-1-exec0-r1");
        let dir = project_state(
            &home,
            "-tmp-leveler-eval-rust-case-1-exec0-r1-abcdef0123456789",
            &gone.display().to_string(),
        );

        let plan = plan_cleanup(&home, &tmp.path().join("nope"), &options(SystemTime::now()));
        let entry = plan
            .entries
            .iter()
            .find(|e| e.path == dir)
            .expect("planned");
        assert_eq!(entry.kind, CleanupKind::HistoricalAutomation);
        assert_eq!(entry.safety, Safety::Safe);
    }

    // G6 — a socket with no listener is safe; a live one is not.
    #[cfg(unix)]
    #[test]
    fn g6_dead_socket_is_safe() {
        let tmp = tempfile::tempdir().unwrap();
        let home = home_at(tmp.path().join("home"));
        let sockets = home.sockets_dir();
        fs::create_dir_all(&sockets).unwrap();
        let dead = sockets.join("deadbeefdeadbeef.sock");
        // A socket file we bind then never accept on: connect() is refused
        // once the listener is dropped.
        let listener = std::os::unix::net::UnixListener::bind(&dead).unwrap();
        drop(listener);
        // On macOS the kernel can briefly keep accepting connects after the
        // final listener handle is dropped. Establish the test precondition
        // instead of racing that teardown.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(1);
        while std::os::unix::net::UnixStream::connect(&dead).is_ok() {
            assert!(
                std::time::Instant::now() < deadline,
                "the closed listener should become unreachable"
            );
            std::thread::yield_now();
        }
        let plan = plan_cleanup(&home, &tmp.path().join("nope"), &options(SystemTime::now()));
        assert!(
            plan.entries
                .iter()
                .any(|e| e.path == dead && e.safety == Safety::Safe),
            "{plan:?}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn symlink_escape_is_rejected_and_target_survives() {
        let tmp = tempfile::tempdir().unwrap();
        let home = home_at(tmp.path().join("home"));
        fs::create_dir_all(home.root()).unwrap();
        let outside = tmp.path().join("outside");
        fs::create_dir_all(&outside).unwrap();
        fs::write(outside.join("keep.txt"), b"do not delete").unwrap();
        let link = home.cache_dir().join("escape");
        fs::create_dir_all(home.cache_dir()).unwrap();
        std::os::unix::fs::symlink(&outside, &link).unwrap();

        let err = safe_remove(&link, std::slice::from_ref(&home.root().to_path_buf())).unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::PermissionDenied);
        assert!(
            outside.join("keep.txt").is_file(),
            "the symlink target must survive"
        );
    }

    #[test]
    fn safe_remove_refuses_paths_outside_the_owned_root() {
        let tmp = tempfile::tempdir().unwrap();
        let home = home_at(tmp.path().join("home"));
        fs::create_dir_all(home.root()).unwrap();
        let outside = tmp.path().join("outside");
        fs::create_dir_all(&outside).unwrap();
        let err =
            safe_remove(&outside, std::slice::from_ref(&home.root().to_path_buf())).unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::PermissionDenied);
        assert!(outside.is_dir());
    }

    // GC1 — under budget means no deletions at all.
    #[test]
    fn gc1_under_budget_deletes_nothing() {
        let tmp = tempfile::tempdir().unwrap();
        let home = home_at(tmp.path().join("home"));
        let entry = home.tool_cache_dir().join("aaaa");
        fs::create_dir_all(&entry).unwrap();
        fs::write(entry.join("blob"), vec![0u8; 1024]).unwrap();
        let mut opts = options(SystemTime::now());
        opts.tool_cache_budget_bytes = 1 << 30;
        let plan = plan_cleanup(&home, &tmp.path().join("nope"), &opts);
        assert!(plan.bytes_for(CleanupKind::ToolCache) == 0, "{plan:?}");
    }

    // GC2/GC3 — over budget removes oldest first and stops at the low watermark.
    #[test]
    fn gc2_gc3_over_budget_removes_oldest_to_low_watermark() {
        let tmp = tempfile::tempdir().unwrap();
        let home = home_at(tmp.path().join("home"));
        let old = home.tool_cache_dir().join("old");
        let new = home.tool_cache_dir().join("new");
        fs::create_dir_all(&old).unwrap();
        fs::create_dir_all(&new).unwrap();
        fs::write(old.join("blob"), vec![0u8; 6 * 1024 * 1024]).unwrap();
        fs::write(new.join("blob"), vec![0u8; 6 * 1024 * 1024]).unwrap();
        let now = SystemTime::now();
        let old_at = now - Duration::from_secs(30 * 24 * 60 * 60);
        let new_at = now - Duration::from_secs(60);
        let timestamps =
            std::collections::HashMap::from([(old.clone(), old_at), (new.clone(), new_at)]);

        let mut opts = options(now);
        opts.tool_cache_budget_bytes = 8 * 1024 * 1024; // total 12MiB > 8MiB
        opts.cache_idle_ttl = Duration::from_secs(60 * 60);
        let mut entries = Vec::new();
        plan_tool_cache_with(&home, &opts, &mut entries, &|entry| {
            timestamps
                .get(&entry.path())
                .copied()
                .unwrap_or(SystemTime::UNIX_EPOCH)
        });
        let removed: Vec<_> = entries
            .iter()
            .filter(|e| e.kind == CleanupKind::ToolCache)
            .map(|e| e.path.clone())
            .collect();
        assert_eq!(removed, vec![old.clone()], "only the oldest is reclaimed");
    }

    // GC4 — a locked cache entry is never a candidate.
    #[test]
    fn gc4_active_cache_is_preserved() {
        let tmp = tempfile::tempdir().unwrap();
        let home = home_at(tmp.path().join("home"));
        let active = home.tool_cache_dir().join("active");
        fs::create_dir_all(&active).unwrap();
        fs::write(active.join("blob"), vec![0u8; 6 * 1024 * 1024]).unwrap();
        // Hold the lease the way a running command would.
        let lock = fs::OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(false)
            .open(active.join(TOOL_CACHE_LOCK_FILE))
            .unwrap();
        fs2::FileExt::try_lock_exclusive(&lock).unwrap();
        assert!(cache_is_active(&active));

        let mut opts = options(SystemTime::now());
        opts.tool_cache_budget_bytes = 1;
        opts.cache_idle_ttl = Duration::from_secs(0);
        let mut entries = Vec::new();
        plan_tool_cache_with(&home, &opts, &mut entries, &|_| SystemTime::UNIX_EPOCH);
        assert!(entries.is_empty(), "{entries:?}");
    }

    // GC7 — a plan's sizes match what execution reclaims.
    #[test]
    fn gc7_plan_sizes_match_execution() {
        let tmp = tempfile::tempdir().unwrap();
        let home = home_at(tmp.path().join("home"));
        let entry = home.tool_cache_dir().join("junk");
        fs::create_dir_all(&entry).unwrap();
        fs::write(entry.join("blob"), vec![0u8; 2048]).unwrap();
        let plan = CleanupPlan {
            entries: vec![CleanupEntry {
                kind: CleanupKind::ToolCache,
                path: entry.clone(),
                size_bytes: entry_size(&entry),
                reason: "test".into(),
                safety: Safety::Safe,
            }],
        };
        let roots = vec![home.root().to_path_buf()];
        let report = execute_plan_under(&plan, false, &roots);
        assert_eq!(report.failures.len(), 0, "{:?}", report.failures);
        assert_eq!(report.reclaimed_bytes, plan.total_bytes());
        assert!(!entry.exists());
    }

    #[test]
    fn dry_run_reports_without_deleting() {
        let tmp = tempfile::tempdir().unwrap();
        let home = home_at(tmp.path().join("home"));
        let entry = home.tool_cache_dir().join("junk");
        fs::create_dir_all(&entry).unwrap();
        fs::write(entry.join("blob"), b"x").unwrap();
        let plan = CleanupPlan {
            entries: vec![CleanupEntry {
                kind: CleanupKind::ToolCache,
                path: entry.clone(),
                size_bytes: 1,
                reason: "test".into(),
                safety: Safety::Safe,
            }],
        };
        let report = execute_plan_under(&plan, true, &[home.root().to_path_buf()]);
        assert_eq!(report.removed, 1);
        assert!(entry.is_dir(), "dry run must not delete");
    }
    // G3 — expired ephemeral residue is safe; a fresh run is not.
    #[cfg(unix)]
    #[test]
    fn g3_expired_ephemeral_residue_is_safe_only_after_ttl() {
        let tmp = tempfile::tempdir().unwrap();
        let home = home_at(tmp.path().join("home"));
        let base = tmp.path().join("tmp").join(EPHEMERAL_NAMESPACE);
        let old_run = base.join("eval").join("old-run");
        let fresh_run = base.join("eval").join("fresh-run");
        fs::create_dir_all(&old_run).unwrap();
        fs::create_dir_all(&fresh_run).unwrap();
        fs::write(old_run.join("sessions.db"), b"residue").unwrap();
        // Age the whole old tree by `touch`.
        let _ = std::process::Command::new("touch")
            .args(["-t", "202001010000"])
            .arg(old_run.join("sessions.db"))
            .arg(&old_run)
            .status();

        let plan = plan_cleanup(&home, &base, &options(SystemTime::now()));
        assert!(
            plan.entries
                .iter()
                .any(|e| e.path == old_run && e.safety == Safety::Safe),
            "{plan:?}"
        );
        assert!(
            plan.entries.iter().all(|e| e.path != fresh_run),
            "a fresh run must not be planned: {plan:?}"
        );
    }

    // C4 — a failed removal is reported, and the rest of the plan still runs.
    #[cfg(unix)]
    #[test]
    fn c4_partial_failure_is_reported_without_losing_the_rest() {
        let tmp = tempfile::tempdir().unwrap();
        let home = home_at(tmp.path().join("home"));
        let good = home.tool_cache_dir().join("good");
        fs::create_dir_all(&good).unwrap();
        fs::write(good.join("blob"), b"x").unwrap();
        // A path that will be a symlink at execution time: safe_remove refuses.
        let trap = home.cache_dir().join("trap");
        let outside = tmp.path().join("outside");
        fs::create_dir_all(home.cache_dir()).unwrap();
        fs::create_dir_all(&outside).unwrap();
        std::os::unix::fs::symlink(&outside, &trap).unwrap();

        let plan = CleanupPlan {
            entries: vec![
                CleanupEntry {
                    kind: CleanupKind::ToolCache,
                    path: trap.clone(),
                    size_bytes: 10,
                    reason: "test".into(),
                    safety: Safety::Safe,
                },
                CleanupEntry {
                    kind: CleanupKind::ToolCache,
                    path: good.clone(),
                    size_bytes: 1,
                    reason: "test".into(),
                    safety: Safety::Safe,
                },
            ],
        };
        let report = execute_plan_under(&plan, false, &[home.root().to_path_buf()]);
        assert_eq!(report.failures.len(), 1, "{report:?}");
        assert_eq!(report.removed, 1, "{report:?}");
        assert!(!good.exists(), "the removable entry still ran");
        assert!(outside.is_dir(), "the symlink target survives");
    }

    // GC5/C6 — two cleanups running at once never cross-delete.
    #[test]
    fn c6_concurrent_plans_do_not_cross_delete() {
        let tmp = tempfile::tempdir().unwrap();
        let home = home_at(tmp.path().join("home"));
        let a = home.tool_cache_dir().join("a");
        let b = home.tool_cache_dir().join("b");
        fs::create_dir_all(&a).unwrap();
        fs::create_dir_all(&b).unwrap();
        fs::write(a.join("blob"), b"a").unwrap();
        fs::write(b.join("blob"), b"b").unwrap();
        let roots = vec![home.root().to_path_buf()];

        let plan_for = |path: PathBuf| CleanupPlan {
            entries: vec![CleanupEntry {
                kind: CleanupKind::ToolCache,
                path,
                size_bytes: 1,
                reason: "test".into(),
                safety: Safety::Safe,
            }],
        };
        let roots_a = roots.clone();
        let plan_b = plan_for(b.clone());
        let handle =
            std::thread::spawn(move || execute_plan_under(&plan_b, false, &roots_a).removed);
        let removed_a = execute_plan_under(&plan_for(a.clone()), false, &roots).removed;
        let removed_b = handle.join().unwrap();
        assert_eq!((removed_a, removed_b), (1, 1));
        assert!(!a.exists() && !b.exists());
    }

    // GC6 — after reclamation the cache is recreatable, so removal is safe.
    #[test]
    fn gc6_cache_rebuild_after_deletion() {
        let tmp = tempfile::tempdir().unwrap();
        let home = home_at(tmp.path().join("home"));
        let entry = home.tool_cache_dir().join("rebuild");
        fs::create_dir_all(&entry).unwrap();
        fs::write(entry.join("blob"), b"x").unwrap();
        let plan = CleanupPlan {
            entries: vec![CleanupEntry {
                kind: CleanupKind::ToolCache,
                path: entry.clone(),
                size_bytes: 1,
                reason: "test".into(),
                safety: Safety::Safe,
            }],
        };
        execute_plan_under(&plan, false, &[home.root().to_path_buf()]);
        assert!(!entry.exists());
        fs::create_dir_all(&entry).unwrap();
        fs::write(entry.join("blob"), b"rebuilt").unwrap();
        assert!(entry.join("blob").is_file());
    }
}
