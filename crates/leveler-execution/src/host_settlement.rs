//! Runtime-owned durable semantic settlement; never part of the process protocol.
//!
//! Admission is committed before spawn. The process host records OS exit, and
//! the runtime calls `reconcile` only after that exit fact is established. A
//! failed diff retains the pending record and write ownership. Diffing is a
//! read-only operation that may be retried after a crash; an atomic journal
//! commit selects one result. This is not an exactly-once external side effect.

use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};

use fs2::FileExt;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::{BackgroundSettlement, MutationBaseline, WorkspaceSnapshot, WriteScope};

const JOURNAL_VERSION: u32 = 1;

/// Bounded wait for a semantic owner lock, and the poll interval used inside
/// it. See [`SettlementJournal::lock`]: the budget covers a transient
/// inherited-description window, not a concurrent diff.
const LOCK_RETRY_BUDGET: std::time::Duration = std::time::Duration::from_millis(1000);
const LOCK_RETRY_INTERVAL: std::time::Duration = std::time::Duration::from_millis(2);

/// Semantic inputs belong to the runtime, including session and writer identity.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SemanticTask {
    version: u32,
    pub task_id: String,
    pub owner: String,
    pub writer: String,
    /// Retained even when a non-Git workspace has no mutation baseline.
    pub write_scope: WriteScope,
    pub baseline: Option<MutationBaseline>,
    pub settlement: Option<BackgroundSettlement>,
}

#[derive(Debug, Clone)]
pub struct SettlementJournal {
    root: PathBuf,
}

impl SettlementJournal {
    /// The caller chooses its private runtime/session state directory outside
    /// the workspace. Execution Host must never read this directory.
    pub fn open(root: &Path) -> Result<Self, String> {
        std::fs::create_dir_all(root).map_err(|e| format!("create semantic journal: {e}"))?;
        Ok(Self {
            root: root.to_path_buf(),
        })
    }

    /// Persist all inputs before asking the host to spawn. A pinned Git ref
    /// makes the baseline survive GC as well as runtime loss. Repeated identical
    /// admission is safe; a reused id with different authority is rejected.
    pub async fn prepare(
        &self,
        task_id: &str,
        owner: &str,
        writer: &str,
        baseline: Option<MutationBaseline>,
    ) -> Result<(), String> {
        let scope = baseline
            .as_ref()
            .map(|b| b.write_scope.clone())
            .unwrap_or(WriteScope::Unrestricted);
        self.prepare_with_scope(task_id, owner, writer, baseline, scope)
            .await
    }

    pub async fn prepare_with_scope(
        &self,
        task_id: &str,
        owner: &str,
        writer: &str,
        baseline: Option<MutationBaseline>,
        write_scope: WriteScope,
    ) -> Result<(), String> {
        if task_id.is_empty() || owner.is_empty() || writer.is_empty() {
            return Err("semantic admission requires task, session and writer identity".into());
        }
        if baseline
            .as_ref()
            .is_some_and(|b| b.write_scope != write_scope)
        {
            return Err("mutation baseline and admitted write scope disagree".into());
        }
        let _lock = self.lock(task_id)?;
        let record = SemanticTask {
            version: JOURNAL_VERSION,
            task_id: task_id.into(),
            owner: owner.into(),
            writer: writer.into(),
            write_scope,
            baseline,
            settlement: None,
        };
        if self.path(task_id).exists() {
            let existing = self.read(task_id)?;
            if existing.owner != record.owner
                || existing.writer != record.writer
                || existing.write_scope != record.write_scope
                || serde_json::to_vec(&existing.baseline).map_err(|e| e.to_string())?
                    != serde_json::to_vec(&record.baseline).map_err(|e| e.to_string())?
            {
                return Err("task id already has different semantic admission".into());
            }
            return Ok(());
        }
        if let Some(baseline) = &record.baseline {
            pin_baseline(task_id, baseline).await?;
        }
        self.commit(&record)
    }

    pub fn read(&self, task_id: &str) -> Result<SemanticTask, String> {
        let bytes = std::fs::read(self.path(task_id))
            .map_err(|e| format!("read semantic admission {task_id}: {e}"))?;
        let record: SemanticTask = serde_json::from_slice(&bytes)
            .map_err(|e| format!("invalid semantic admission {task_id}: {e}"))?;
        if record.version != JOURNAL_VERSION || record.task_id != task_id {
            return Err(format!("incompatible semantic admission {task_id}"));
        }
        Ok(record)
    }

    /// Includes tasks whose process has exited but whose settlement is pending.
    /// Their namespace stays reserved until the result is durably committed.
    pub fn pending(&self) -> Result<Vec<SemanticTask>, String> {
        let mut records = Vec::new();
        for entry in std::fs::read_dir(&self.root).map_err(|e| e.to_string())? {
            let path = entry.map_err(|e| e.to_string())?.path();
            if path.extension().and_then(|s| s.to_str()) != Some("json") {
                continue;
            }
            let bytes = std::fs::read(&path).map_err(|e| e.to_string())?;
            let record: SemanticTask = serde_json::from_slice(&bytes)
                .map_err(|e| format!("invalid semantic journal {}: {e}", path.display()))?;
            if record.version != JOURNAL_VERSION || self.path(&record.task_id) != path {
                return Err(format!("incompatible semantic journal {}", path.display()));
            }
            if record.settlement.is_none() {
                records.push(record);
            }
        }
        records.sort_by(|a, b| a.task_id.cmp(&b.task_id));
        Ok(records)
    }

    /// Precondition: the host durably proved the entire process group exited.
    /// A concurrent runtime gets a retryable lock error; it never runs a second
    /// simultaneous diff. Subsequent calls return the committed result.
    pub async fn reconcile(&self, task_id: &str) -> Result<BackgroundSettlement, String> {
        let _lock = self.lock(task_id)?;
        let mut record = self.read(task_id)?;
        if let Some(settlement) = record.settlement {
            return Ok(settlement);
        }
        let settlement = match &record.baseline {
            Some(baseline) => BackgroundSettlement {
                modified: WorkspaceSnapshot::changed_since_for_scope(
                    &baseline.workspace_root,
                    &baseline.snapshot,
                    &baseline.write_scope,
                )
                .await
                .map_err(|e| {
                    format!("settlement pending for {task_id}: cannot diff baseline: {e}")
                })?,
                violation: None,
                note: None,
                snapshot: (!matches!(baseline.write_scope, WriteScope::ScopedWorkspace { .. }))
                    .then(|| baseline.snapshot.clone()),
            },
            None => BackgroundSettlement {
                modified: Vec::new(),
                violation: None,
                note: Some(
                    "No workspace baseline was available at admission; file changes are unknown"
                        .into(),
                ),
                snapshot: None,
            },
        };
        record.settlement = Some(settlement.clone());
        self.commit(&record)?;
        // Keep the Git ref: the published snapshot remains a valid recovery
        // target. Record retention and ref pruning must share a later boundary.
        Ok(settlement)
    }

    /// Only for a spawn that is definitively rejected (never for an ambiguous
    /// transport failure). An uncertain spawn retains its write reservation.
    pub fn abandon_prepared(&self, task_id: &str) -> Result<(), String> {
        let _lock = self.lock(task_id)?;
        let record = self.read(task_id)?;
        if record.settlement.is_some() {
            return Err("cannot abandon a settled task".into());
        }
        std::fs::remove_file(self.path(task_id)).map_err(|e| e.to_string())?;
        sync_directory(&self.root)
    }

    fn path(&self, task_id: &str) -> PathBuf {
        self.root.join(format!("{}.json", key(task_id)))
    }

    /// Take this task's semantic owner lock without waiting for a *runtime*
    /// that is already diffing: the request is answered with a retryable error
    /// rather than queueing a second simultaneous diff.
    ///
    /// The short retry budget exists because a lost `flock` is not always
    /// another runtime. A child forked by this process between `fork` and
    /// `exec` holds a transient copy of the open file description, so a lock
    /// request that lands in that window would otherwise report an owner that
    /// does not exist. The window is tiny; a real concurrent runtime holds the
    /// lock for a whole diff and still gets the error.
    fn lock(&self, task_id: &str) -> Result<File, String> {
        let path = self.root.join(format!("{}.lock", key(task_id)));
        let deadline = std::time::Instant::now() + LOCK_RETRY_BUDGET;
        loop {
            let file = OpenOptions::new()
                .create(true)
                .truncate(false)
                .read(true)
                .write(true)
                .open(&path)
                .map_err(|e| format!("open semantic owner lock: {e}"))?;
            match FileExt::try_lock_exclusive(&file) {
                Ok(()) => return Ok(file),
                Err(error) => {
                    if std::time::Instant::now() >= deadline {
                        return Err(format!(
                            "semantic reconciliation busy or lock unavailable for {task_id}: {error}"
                        ));
                    }
                    std::thread::sleep(LOCK_RETRY_INTERVAL);
                }
            }
        }
    }

    fn commit(&self, record: &SemanticTask) -> Result<(), String> {
        let mut temp = tempfile::NamedTempFile::new_in(&self.root).map_err(|e| e.to_string())?;
        let bytes = serde_json::to_vec(record).map_err(|e| e.to_string())?;
        temp.write_all(&bytes).map_err(|e| e.to_string())?;
        temp.as_file().sync_all().map_err(|e| e.to_string())?;
        temp.persist(self.path(&record.task_id))
            .map_err(|e| e.to_string())?;
        sync_directory(&self.root)
    }
}

fn key(task_id: &str) -> String {
    format!("{:x}", Sha256::digest(task_id.as_bytes()))
}

fn sync_directory(path: &Path) -> Result<(), String> {
    #[cfg(unix)]
    File::open(path)
        .and_then(|f| f.sync_all())
        .map_err(|e| format!("sync semantic directory: {e}"))?;
    #[cfg(not(unix))]
    let _ = path;
    Ok(())
}

async fn pin_baseline(task_id: &str, baseline: &MutationBaseline) -> Result<(), String> {
    let output = tokio::process::Command::new("git")
        .current_dir(&baseline.workspace_root)
        .args([
            "update-ref",
            &format!("refs/leveler/background/{}", key(task_id)),
            &baseline.snapshot.0,
        ])
        .output()
        .await
        .map_err(|e| format!("pin settlement baseline: {e}"))?;
    if !output.status.success() {
        return Err(format!(
            "pin settlement baseline: {}",
            String::from_utf8_lossy(&output.stderr)
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{MutationBaseline, WorkspaceSnapshot, WriteScope};

    fn repository() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        assert!(
            std::process::Command::new("git")
                .args(["init", "-q"])
                .arg(dir.path())
                .status()
                .unwrap()
                .success()
        );
        std::fs::write(dir.path().join("file.txt"), "before").unwrap();
        dir
    }

    #[tokio::test]
    async fn offline_exit_reconciles_durable_baseline_and_commits_once() {
        let repo = repository();
        let state = tempfile::tempdir().unwrap();
        let baseline = MutationBaseline {
            snapshot: WorkspaceSnapshot::capture(repo.path())
                .await
                .unwrap()
                .unwrap(),
            workspace_root: repo.path().to_path_buf(),
            write_scope: WriteScope::Workspace {
                root: repo.path().to_path_buf(),
            },
        };
        let journal = SettlementJournal::open(state.path()).unwrap();
        journal
            .prepare("host-offline", "session", "writer", Some(baseline))
            .await
            .unwrap();
        drop(journal);
        std::fs::write(repo.path().join("file.txt"), "while runtime offline").unwrap();
        let journal = SettlementJournal::open(state.path()).unwrap();
        assert_eq!(journal.pending().unwrap().len(), 1);
        let first = journal.reconcile("host-offline").await.unwrap();
        assert_eq!(first.modified, vec!["file.txt"]);
        std::fs::write(repo.path().join("later.txt"), "unrelated later write").unwrap();
        let again = journal.reconcile("host-offline").await.unwrap();
        assert_eq!(again.modified, first.modified);
        assert!(journal.pending().unwrap().is_empty());
    }

    #[tokio::test]
    async fn admission_pins_baseline_against_git_gc() {
        let repo = repository();
        let state = tempfile::tempdir().unwrap();
        let baseline = MutationBaseline {
            snapshot: WorkspaceSnapshot::capture(repo.path())
                .await
                .unwrap()
                .unwrap(),
            workspace_root: repo.path().to_path_buf(),
            write_scope: WriteScope::Workspace {
                root: repo.path().to_path_buf(),
            },
        };
        let journal = SettlementJournal::open(state.path()).unwrap();
        journal
            .prepare("host-gc", "session", "writer", Some(baseline))
            .await
            .unwrap();
        assert!(
            std::process::Command::new("git")
                .current_dir(repo.path())
                .args(["prune", "--expire=now"])
                .status()
                .unwrap()
                .success()
        );
        std::fs::write(repo.path().join("file.txt"), "after gc").unwrap();
        assert_eq!(
            journal.reconcile("host-gc").await.unwrap().modified,
            vec!["file.txt"]
        );
    }

    #[tokio::test]
    async fn only_one_runtime_can_reconcile_and_diff_failure_retains_ownership() {
        let repo = repository();
        let state = tempfile::tempdir().unwrap();
        let baseline = MutationBaseline {
            snapshot: WorkspaceSnapshot::capture(repo.path())
                .await
                .unwrap()
                .unwrap(),
            workspace_root: repo.path().to_path_buf(),
            write_scope: WriteScope::Workspace {
                root: repo.path().to_path_buf(),
            },
        };
        let journal = SettlementJournal::open(state.path()).unwrap();
        journal
            .prepare("host-owner", "session", "writer", Some(baseline))
            .await
            .unwrap();
        let runtime_b = SettlementJournal::open(state.path()).unwrap();
        let lock = journal.lock("host-owner").unwrap();
        assert!(
            runtime_b
                .reconcile("host-owner")
                .await
                .unwrap_err()
                .contains("busy")
        );
        drop(lock);
        std::fs::remove_dir_all(repo.path().join(".git")).unwrap();
        assert!(
            runtime_b
                .reconcile("host-owner")
                .await
                .unwrap_err()
                .contains("settlement pending")
        );
        let pending = runtime_b.pending().unwrap();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].owner, "session");
        assert_eq!(pending[0].writer, "writer");
        assert!(pending[0].settlement.is_none());
    }

    #[tokio::test]
    async fn admission_without_snapshot_retains_scope_and_rejects_conflicting_retry() {
        let state = tempfile::tempdir().unwrap();
        let scope = WriteScope::Workspace {
            root: state.path().join("workspace"),
        };
        let journal = SettlementJournal::open(state.path()).unwrap();
        journal
            .prepare_with_scope("host-no-git", "session", "writer", None, scope.clone())
            .await
            .unwrap();
        assert_eq!(journal.pending().unwrap()[0].write_scope, scope);
        assert!(
            journal
                .prepare_with_scope("host-no-git", "other", "writer", None, scope)
                .await
                .is_err()
        );
        let settlement = journal.reconcile("host-no-git").await.unwrap();
        assert!(settlement.note.unwrap().contains("unknown"));
    }
}
