//! Markdown-backed memory store.
//!
//! Stage 1 implements the storage and lifecycle foundation only. It is not
//! wired into the agent runtime, the TUI, the desktop app, the legacy store, or
//! any FTS index.

use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use super::frontmatter::{self, DocumentError};
use super::journal::{
    AtticEntry, JournalPlan, JournalState, NewFilePlan, RecoveryReport, StateFileKind,
    StateFilePlan, TargetDisposition, TargetPlan,
};
use super::lifecycle::{
    self, Admission, CreationDecision, PendingReason, RejectReason, SkipReason, TargetDecision,
    TargetView,
};
use super::path::{self, PathError, ScopeLock, ScopeRoots, StorePaths};
use super::receipt::{self, Receipt, Trust, TrustView};
use super::record::{
    Caller, Cas, Experience, Frontmatter, IntentRecord, MemoryAuthority, MemoryId, MemoryIntent,
    MemoryKind, MemoryRecord, MemoryScope, MemoryStatus, OpId, Operation, PinOwner, Proposal,
    Relation, SourceRef, SourceRefKind, StatementKind, Tombstone,
};

/// Process-wide lifecycle counters for the Markdown store.
///
/// These answer "how much mechanical work did this process do to a store?"
/// and nothing about memory content: they are never persisted, never read by
/// retrieval, and never change behaviour. The scale benchmark and the user's
/// own diagnostics read them to prove that an operation did not silently
/// re-open a store or re-scan the state tree.
#[derive(Debug, Default)]
pub struct StoreLifecycleMetrics {
    opens: AtomicU64,
    journal_recoveries: AtomicU64,
    orphan_cleanups: AtomicU64,
    reads: AtomicU64,
}

/// One point-in-time reading of [`StoreLifecycleMetrics`].
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct StoreLifecycleSnapshot {
    pub opens: u64,
    pub journal_recoveries: u64,
    pub orphan_cleanups: u64,
    pub reads: u64,
}

impl StoreLifecycleMetrics {
    pub fn snapshot(&self) -> StoreLifecycleSnapshot {
        StoreLifecycleSnapshot {
            opens: self.opens.load(Ordering::Relaxed),
            journal_recoveries: self.journal_recoveries.load(Ordering::Relaxed),
            orphan_cleanups: self.orphan_cleanups.load(Ordering::Relaxed),
            reads: self.reads.load(Ordering::Relaxed),
        }
    }

    /// Reset every counter. Only tests do this; there is no production reason
    /// to forget how much work already happened.
    pub fn reset(&self) {
        for counter in [
            &self.opens,
            &self.journal_recoveries,
            &self.orphan_cleanups,
            &self.reads,
        ] {
            counter.store(0, Ordering::Relaxed);
        }
    }

    fn count(counter: &AtomicU64) {
        counter.fetch_add(1, Ordering::Relaxed);
    }
}

static STORE_METRICS: StoreLifecycleMetrics = StoreLifecycleMetrics {
    opens: AtomicU64::new(0),
    journal_recoveries: AtomicU64::new(0),
    orphan_cleanups: AtomicU64::new(0),
    reads: AtomicU64::new(0),
};

/// The process-wide store lifecycle counters.
pub fn lifecycle_metrics() -> &'static StoreLifecycleMetrics {
    &STORE_METRICS
}

#[derive(Debug, Clone, Default)]
pub struct ListQuery {
    pub scope: Option<MemoryScope>,
    pub kind: Option<MemoryKind>,
    pub status: Option<MemoryStatus>,
    pub limit: Option<usize>,
    pub include_external: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MemorySummary {
    pub id: MemoryId,
    pub kind: MemoryKind,
    pub scope: MemoryScope,
    pub status: MemoryStatus,
    pub revision: u64,
    pub title: String,
    pub subject: Option<String>,
    pub subject_key: Option<String>,
    pub value: Option<String>,
    pub tags: Vec<String>,
    pub authority: MemoryAuthority,
    pub effective_authority: MemoryAuthority,
    pub trust: TrustSummary,
    pub expires_at: Option<DateTime<Utc>>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TrustSummary {
    Trusted,
    ExternalUnverified,
    ExternalConflict,
}

impl From<Trust> for TrustSummary {
    fn from(trust: Trust) -> Self {
        match trust {
            Trust::Trusted => Self::Trusted,
            Trust::ExternalUnverified => Self::ExternalUnverified,
            Trust::ExternalConflict => Self::ExternalConflict,
        }
    }
}

#[derive(Debug, Clone)]
pub struct ReadRecord {
    pub record: MemoryRecord,
    pub digest: String,
    pub cas: Cas,
    pub trust: TrustSummary,
    pub effective_authority: MemoryAuthority,
}

#[derive(Debug, Clone)]
pub struct EffectiveRecord {
    pub record: MemoryRecord,
    pub digest: String,
    pub cas: Cas,
    pub effective_authority: MemoryAuthority,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct RecordPatch {
    pub title: Option<String>,
    pub body: Option<String>,
    pub value: Option<String>,
    pub tags: Option<Vec<String>>,
    pub expires_at: Option<Option<DateTime<Utc>>>,
    pub evidence: Option<Vec<SourceRef>>,
    /// Attach a structured identity to an explicit record. `None` leaves the
    /// current identity unchanged; an identity is never cleared or replaced
    /// here (use an explicit supersede for that).
    pub subject: Option<String>,
    pub subject_key: Option<String>,
}

#[derive(Debug, Clone)]
pub struct NewRecord {
    pub kind: MemoryKind,
    pub scope: MemoryScope,
    pub subject: Option<String>,
    pub subject_key: Option<String>,
    pub value: Option<String>,
    pub title: String,
    pub body: String,
    pub tags: Vec<String>,
    pub statement: StatementKind,
    pub expires_at: Option<DateTime<Utc>>,
    pub evidence: Vec<SourceRef>,
    pub experience: Option<Experience>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Reason(pub String);

impl Reason {
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub enum ReviewDecision {
    Accept,
    Reject { reason: Reason },
}

#[derive(Debug, Clone)]
pub enum ForgetOutcome {
    Forgotten(Tombstone),
    Pending(Admission),
    NotApplied(Admission),
}

#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("memory lock is busy")]
    Busy,
    #[error("compare-and-swap mismatch for {target}")]
    Conflict {
        target: MemoryId,
        current: Cas,
        expected: Cas,
    },
    #[error("external conflict for {id}: {reason}")]
    ExternalConflict { id: MemoryId, reason: String },
    #[error("journal recovery conflict: {reason}")]
    RecoveryConflict { op_id: Option<OpId>, reason: String },
    #[error("operation id was reused with a different request: {0}")]
    OpIdReuse(OpId),
    #[error("memory not found: {0}")]
    NotFound(MemoryId),
    #[error("memory was forgotten: {0}")]
    Forgotten(MemoryId),
    #[error("memory is not current: {id} ({status:?})")]
    NotCurrent { id: MemoryId, status: MemoryStatus },
    #[error("actor is not allowed to perform this action: {0}")]
    ForbiddenActor(String),
    #[error("identity invariant violated for subject_key {subject_key}: {ids:?}")]
    InvariantViolation {
        subject_key: String,
        ids: Vec<MemoryId>,
    },
    #[error("invalid memory operation: {0}")]
    Invalid(String),
    #[error(transparent)]
    Document(#[from] DocumentError),
    #[error(transparent)]
    Path(PathError),
}

impl From<PathError> for StoreError {
    fn from(error: PathError) -> Self {
        match error {
            PathError::LockTimeout => Self::Busy,
            other => Self::Path(other),
        }
    }
}

/// How badly a diagnostic finding blocks the scope.
///
/// The order is the output order: the state a person must resolve first comes
/// first.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DiagnosticSeverity {
    /// The scope cannot be read or written correctly until a person acts.
    Blocking,
    /// Reads work, but some truth is withheld until a person decides.
    ActionRequired,
    /// The store heals this on its next read or write; shown for visibility.
    Warning,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DiagnosticKind {
    BlockedJournal,
    PendingJournal,
    InvalidJournal,
    InvalidCurrentDocument,
    NonActiveCurrentRecord,
    ScopeMismatch,
    ExternalConflict,
    ExternalUnverified,
    InvalidReceipt,
    DuplicateActiveSubject,
    InvalidPendingDocument,
    OrphanStageFile,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DiagnosticIssue {
    pub severity: DiagnosticSeverity,
    pub kind: DiagnosticKind,
    pub path: Option<PathBuf>,
    pub detail: String,
    pub suggestion: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DiagnosticReport {
    pub scope: MemoryScope,
    pub current_root: PathBuf,
    pub state_root: PathBuf,
    /// Whether both roots exist. A missing store is not an error to inspect.
    pub initialized: bool,
    /// Current files whose content matches their receipt.
    pub active_records: usize,
    pub pending_records: usize,
    pub issues: Vec<DiagnosticIssue>,
}

impl DiagnosticReport {
    pub fn is_blocking(&self) -> bool {
        self.issues
            .iter()
            .any(|issue| issue.severity == DiagnosticSeverity::Blocking)
    }

    pub fn needs_action(&self) -> bool {
        self.issues
            .iter()
            .any(|issue| issue.severity != DiagnosticSeverity::Warning)
    }
}

#[derive(Debug, Clone)]
struct Loaded {
    record: MemoryRecord,
    bytes: Vec<u8>,
    digest: String,
    trust: TrustView,
}

impl Loaded {
    fn cas(&self) -> Cas {
        Cas {
            revision: self.record.frontmatter.revision,
            digest: self.digest.clone(),
        }
    }

    fn view(&self, now: DateTime<Utc>) -> TargetView<'_> {
        TargetView {
            record: &self.record,
            digest: self.digest.clone(),
            trust: self.trust.trust,
            effective_authority: self.trust.effective_authority,
            protected: self.trust.protected,
            expired: self
                .record
                .frontmatter
                .expires_at
                .is_some_and(|expires| expires <= now),
        }
    }
}

#[derive(Debug, Clone)]
struct TargetAction {
    loaded: Loaded,
    disposition: TargetDisposition,
    final_status: Option<MemoryStatus>,
    superseded_by: Option<MemoryId>,
    reason: Option<String>,
}

#[derive(Debug, Clone)]
struct Tx {
    operation: Operation,
    caller: String,
    new_record: Option<(MemoryRecord, Vec<u8>)>,
    pending_record: Option<(MemoryRecord, Vec<u8>)>,
    targets: Vec<TargetAction>,
    receipt: Option<Receipt>,
    tombstones: Vec<Tombstone>,
    attic: Vec<AtticEntry>,
    pending_deletes: Vec<MemoryId>,
    delete_dirs: Vec<PathBuf>,
    appends: Vec<super::journal::AppendPlan>,
    intent: IntentRecord,
}

#[derive(Debug, Clone)]
struct PreparedFiles {
    stage: Option<PathBuf>,
    state_files: Vec<StateFilePlan>,
}

pub struct MarkdownStore {
    paths: StorePaths,
    scope: MemoryScope,
}

impl MarkdownStore {
    pub fn open(roots: ScopeRoots) -> Result<Self, StoreError> {
        StoreLifecycleMetrics::count(&STORE_METRICS.opens);
        let paths = StorePaths::prepare(&roots)?;
        let store = Self {
            paths,
            scope: roots.scope,
        };
        {
            let _lock = store.lock_exclusive()?;
            let mut report = store.recover_locked()?;
            // Orphan cleanup is a lifecycle event, not a per-read cost: it scans
            // every state directory (and one directory per historical record).
            // It belongs to opening a store and to an explicit recovery.
            report.cleaned = store.cleanup_orphans()?;
        }
        Ok(store)
    }

    pub fn scope(&self) -> MemoryScope {
        self.scope
    }

    pub fn paths(&self) -> &StorePaths {
        &self.paths
    }

    pub fn read(&self, id: &MemoryId) -> Result<ReadRecord, StoreError> {
        StoreLifecycleMetrics::count(&STORE_METRICS.reads);
        let _lock = self.lock_shared_recovered()?;
        self.read_locked(id)
    }

    /// Read several current records under one shared lock and one recovery
    /// check.
    ///
    /// Results are positional: `results[position]` belongs to `ids[position]`.
    /// Each slot carries its own outcome, so one missing, non-current or
    /// conflicting record never hides the others and never becomes an empty
    /// success. The lock failure is the only outer error: without it nothing was
    /// read.
    pub fn read_many(
        &self,
        ids: &[MemoryId],
    ) -> Result<Vec<Result<ReadRecord, StoreError>>, StoreError> {
        let _lock = self.lock_shared_recovered()?;
        Ok(ids
            .iter()
            .map(|id| {
                StoreLifecycleMetrics::count(&STORE_METRICS.reads);
                self.read_locked(id)
            })
            .collect())
    }

    /// The read body; the caller holds the shared lock.
    fn read_locked(&self, id: &MemoryId) -> Result<ReadRecord, StoreError> {
        let loaded = self.load_current(id)?;
        let Some(loaded) = loaded else {
            return self.non_current_error(id);
        };
        if loaded.trust.trust == Trust::ExternalConflict {
            return Err(StoreError::ExternalConflict {
                id: id.clone(),
                reason: "receipt revision, status, or subject_key no longer matches".into(),
            });
        }
        let cas = loaded.cas();
        Ok(ReadRecord {
            record: loaded.record,
            digest: loaded.digest.clone(),
            cas,
            trust: loaded.trust.trust.into(),
            effective_authority: loaded.trust.effective_authority,
        })
    }

    /// Records retired out of the current root, newest first.
    ///
    /// The attic holds identity and revision; the document itself lives in
    /// history. A forgotten record's body is gone by design, so its missing
    /// history file is skipped rather than reported as a broken archive.
    pub fn archived(&self) -> Result<Vec<MemoryRecord>, StoreError> {
        let entries = {
            let _lock = self.lock_shared_recovered()?;
            self.scan_attic()?
        };
        let mut records = Vec::new();
        for entry in entries {
            match self.history(&entry.id, entry.revision) {
                Ok(record) => records.push(record),
                Err(StoreError::NotFound(_)) | Err(StoreError::Forgotten(_)) => continue,
                Err(error) => return Err(error),
            }
        }
        records.sort_by(|left, right| {
            right
                .frontmatter
                .updated_at
                .cmp(&left.frontmatter.updated_at)
        });
        Ok(records)
    }

    fn scan_attic(&self) -> Result<Vec<AtticEntry>, StoreError> {
        let entries = fs::read_dir(&self.paths.attic_dir).map_err(|error| {
            StoreError::Path(PathError::Io {
                path: self.paths.attic_dir.clone(),
                source: error,
            })
        })?;
        let mut attic = Vec::new();
        for entry in entries {
            let entry = entry.map_err(|error| {
                StoreError::Path(PathError::Io {
                    path: self.paths.attic_dir.clone(),
                    source: error,
                })
            })?;
            let path = entry.path();
            if !entry
                .file_type()
                .map(|kind| kind.is_file())
                .unwrap_or(false)
                || path.extension().and_then(std::ffi::OsStr::to_str) != Some("json")
            {
                continue;
            }
            let id = MemoryId::parse(
                path.file_stem()
                    .and_then(std::ffi::OsStr::to_str)
                    .unwrap_or_default(),
            )
            .map_err(|error| StoreError::Invalid(error.to_string()))?;
            let bytes = path::read_bytes(&path)?;
            attic.push(serde_json::from_slice(&bytes).map_err(|error| {
                StoreError::ExternalConflict {
                    id,
                    reason: format!("attic metadata is invalid: {error}"),
                }
            })?);
        }
        Ok(attic)
    }

    pub fn history(&self, id: &MemoryId, revision: u64) -> Result<MemoryRecord, StoreError> {
        let _lock = self.lock_shared_recovered()?;
        if self.tombstone(id)?.is_some() {
            return Err(StoreError::Forgotten(id.clone()));
        }
        let path = self.paths.history_file(id, revision);
        let bytes = match path::read_bytes(&path) {
            Ok(bytes) => bytes,
            Err(PathError::Io { source, .. }) if source.kind() == std::io::ErrorKind::NotFound => {
                return Err(StoreError::NotFound(id.clone()));
            }
            Err(error) => return Err(error.into()),
        };
        let record = frontmatter::parse(&bytes).map_err(|error| StoreError::ExternalConflict {
            id: id.clone(),
            reason: format!("history document is invalid: {error}"),
        })?;
        if record.frontmatter.id != *id || record.frontmatter.revision != revision {
            return Err(StoreError::ExternalConflict {
                id: id.clone(),
                reason: "history document identity does not match its path".into(),
            });
        }
        Ok(record)
    }

    pub fn list(&self, query: ListQuery) -> Result<Vec<MemorySummary>, StoreError> {
        let _lock = self.lock_shared_recovered()?;
        let now = Utc::now();
        let loaded = self.scan_current(true)?;
        let mut summaries = Vec::new();
        for loaded in loaded {
            if query.scope.is_some_and(|scope| scope != self.scope) {
                continue;
            }
            if query
                .kind
                .is_some_and(|kind| kind != loaded.record.frontmatter.kind)
            {
                continue;
            }
            if query
                .status
                .is_some_and(|status| status != loaded.record.frontmatter.status)
            {
                continue;
            }
            if !query.include_external && loaded.trust.trust != Trust::Trusted {
                continue;
            }
            summaries.push(self.summary(&loaded, now));
        }
        summaries.sort_by(|left, right| left.id.cmp(&right.id));
        if let Some(limit) = query.limit {
            summaries.truncate(limit);
        }
        Ok(summaries)
    }

    pub fn effective_active(&self) -> Result<Vec<EffectiveRecord>, StoreError> {
        let _lock = self.lock_shared_recovered()?;
        let now = Utc::now();
        let mut records = Vec::new();
        for loaded in self.scan_current(true)? {
            if loaded.trust.trust != Trust::Trusted {
                continue;
            }
            if loaded.record.frontmatter.sensitive {
                continue;
            }
            if loaded
                .record
                .frontmatter
                .expires_at
                .is_some_and(|expires| expires <= now)
            {
                continue;
            }
            records.push(EffectiveRecord {
                record: loaded.record.clone(),
                digest: loaded.digest.clone(),
                cas: loaded.cas(),
                effective_authority: loaded.trust.effective_authority,
            });
        }
        records.sort_by(|left, right| left.record.frontmatter.id.cmp(&right.record.frontmatter.id));
        Ok(records)
    }

    pub fn list_pending(&self) -> Result<Vec<MemoryRecord>, StoreError> {
        let _lock = self.lock_shared_recovered()?;
        let mut records = Vec::new();
        for path in path::list_markdown(&self.paths.pending_dir)? {
            let bytes = path::read_bytes(&path)?;
            let record =
                frontmatter::parse(&bytes).map_err(|error| StoreError::RecoveryConflict {
                    op_id: None,
                    reason: format!("pending document is invalid: {error}"),
                })?;
            if record.frontmatter.status != MemoryStatus::Pending
                || record.frontmatter.proposal.is_none()
            {
                return Err(StoreError::RecoveryConflict {
                    op_id: None,
                    reason: format!(
                        "pending document {} has an invalid lifecycle state",
                        record.frontmatter.id
                    ),
                });
            }
            if record.frontmatter.scope != self.scope {
                return Err(StoreError::RecoveryConflict {
                    op_id: None,
                    reason: format!(
                        "pending document {} has the wrong scope",
                        record.frontmatter.id
                    ),
                });
            }
            records.push(record);
        }
        records.sort_by(|left, right| left.frontmatter.id.cmp(&right.frontmatter.id));
        Ok(records)
    }

    pub fn admit(&self, request: AdmitRequest) -> Result<Admission, StoreError> {
        let _lock = self.lock_exclusive_recovered()?;
        self.admit_locked(request)
    }

    /// Park a create as a pending proposal even when the caller path would
    /// normally create it directly.
    ///
    /// This is the unattended path: nobody was available to approve the write,
    /// so the content is kept for the user instead of being applied or
    /// discarded. The pending record carries the same identity the eventual
    /// review will use.
    pub fn propose(
        &self,
        request: AdmitRequest,
        reason: super::lifecycle::PendingReason,
    ) -> Result<Admission, StoreError> {
        if !matches!(request.intent.relation, Relation::None) {
            return Err(StoreError::Invalid(
                "only a create can be parked as a pending proposal".into(),
            ));
        }
        let _lock = self.lock_exclusive_recovered()?;
        let digest = request_digest(&(
            &request.caller,
            &request.intent,
            &request.expected,
            "propose",
            reason.as_str(),
        ))?;
        self.admit_locked_with_digest_mode(request, digest, Some(reason))
    }

    pub fn update(
        &self,
        op: OpId,
        caller: Caller,
        id: &MemoryId,
        patch: RecordPatch,
        expected: Cas,
    ) -> Result<Admission, StoreError> {
        let _lock = self.lock_exclusive_recovered()?;
        let digest = request_digest(&(&caller, id, &patch, &expected))?;
        if let Some(admission) = self.replay_intent(&op, &digest)? {
            return Ok(admission);
        }
        let target = self
            .load_current(id)?
            .ok_or_else(|| StoreError::NotFound(id.clone()))?;
        if target.cas() != expected {
            let admission = Admission::Conflict {
                target: id.clone(),
                current: target.cas(),
                expected,
            };
            self.finish_noop(&op, &digest, admission.clone())?;
            return Ok(admission);
        }
        if target.trust.trust != Trust::Trusted {
            return Err(StoreError::ExternalConflict {
                id: id.clone(),
                reason: "target is not a trusted current record".into(),
            });
        }
        let intent = intent_from_patch(&target.record, &patch);
        let request = AdmitRequest {
            op_id: op,
            caller,
            intent,
            expected: Some(expected),
        };
        self.admit_locked_with_digest(request, digest)
    }

    pub fn supersede(
        &self,
        op: OpId,
        caller: Caller,
        new: NewRecord,
        targets: &[MemoryId],
    ) -> Result<Admission, StoreError> {
        let _lock = self.lock_exclusive_recovered()?;
        let intent = intent_from_new(
            new,
            Relation::Supersede {
                targets: targets.to_vec(),
            },
        );
        let request = AdmitRequest {
            op_id: op,
            caller,
            intent,
            expected: None,
        };
        self.admit_locked(request)
    }

    pub fn deprecate(
        &self,
        op: OpId,
        caller: Caller,
        id: &MemoryId,
        expected: Cas,
        reason: Reason,
    ) -> Result<Admission, StoreError> {
        let _lock = self.lock_exclusive_recovered()?;
        self.destructive_locked(
            op,
            caller,
            id,
            expected,
            DestructiveAction::Deprecate { reason },
        )
    }

    pub fn pin(
        &self,
        op: OpId,
        caller: Caller,
        id: &MemoryId,
        pinned_by: PinOwner,
        expected: Cas,
    ) -> Result<Admission, StoreError> {
        let _lock = self.lock_exclusive_recovered()?;
        self.destructive_locked(
            op,
            caller,
            id,
            expected,
            DestructiveAction::Pin { pinned_by },
        )
    }

    pub fn unpin(
        &self,
        op: OpId,
        caller: Caller,
        id: &MemoryId,
        expected: Cas,
    ) -> Result<Admission, StoreError> {
        let _lock = self.lock_exclusive_recovered()?;
        self.destructive_locked(op, caller, id, expected, DestructiveAction::Unpin)
    }

    pub fn forget(
        &self,
        op: OpId,
        caller: Caller,
        id: &MemoryId,
        expected: Cas,
    ) -> Result<ForgetOutcome, StoreError> {
        let _lock = self.lock_exclusive_recovered()?;
        let admission =
            self.destructive_locked(op, caller, id, expected, DestructiveAction::Forget)?;
        Ok(match admission {
            Admission::Applied {
                operation: Operation::Forget,
                ..
            }
            | Admission::Replayed {
                operation: Operation::Forget,
                ..
            } => match self.tombstone(id)? {
                Some(tombstone) => ForgetOutcome::Forgotten(tombstone),
                None => {
                    return Err(StoreError::RecoveryConflict {
                        op_id: None,
                        reason: "forget committed without a tombstone".into(),
                    });
                }
            },
            Admission::Pending { .. } => ForgetOutcome::Pending(admission),
            other => ForgetOutcome::NotApplied(other),
        })
    }

    pub fn expire_due(&self, now: DateTime<Utc>) -> Result<Vec<MemoryId>, StoreError> {
        let _lock = self.lock_exclusive_recovered()?;
        let mut expired = Vec::new();
        for loaded in self.scan_current(true)? {
            if loaded.trust.trust != Trust::Trusted {
                continue;
            }
            let Some(expires) = loaded.record.frontmatter.expires_at else {
                continue;
            };
            if expires > now {
                continue;
            }
            let op = OpId::parse(format!(
                "expire:{}:{}",
                loaded.record.frontmatter.id, loaded.record.frontmatter.revision
            ))
            .map_err(|error| StoreError::Invalid(error.to_string()))?;
            let digest = request_digest(&("expire", &op, &loaded.digest))?;
            if self.replay_intent(&op, &digest)?.is_some() {
                continue;
            }
            let admission = Admission::Applied {
                id: loaded.record.frontmatter.id.clone(),
                revision: loaded.record.frontmatter.revision,
                operation: Operation::Expire,
                possible_duplicate: Vec::new(),
            };
            let intent_record = IntentRecord {
                op_id: op.clone(),
                request_digest: digest,
                admission: Some(admission),
                created_at: Utc::now().to_rfc3339(),
            };
            let mut tx = Tx {
                operation: Operation::Expire,
                caller: "runtime".into(),
                new_record: None,
                pending_record: None,
                targets: Vec::new(),
                receipt: None,
                tombstones: Vec::new(),
                attic: Vec::new(),
                pending_deletes: Vec::new(),
                delete_dirs: Vec::new(),
                appends: Vec::new(),
                intent: intent_record,
            };
            tx.targets.push(TargetAction {
                loaded: loaded.clone(),
                disposition: TargetDisposition::Retire,
                final_status: Some(MemoryStatus::Expired),
                superseded_by: None,
                reason: Some("ttl".into()),
            });
            tx.attic.push(attic_entry(
                &loaded,
                MemoryStatus::Expired,
                None,
                Some("ttl".into()),
            ));
            self.execute_tx(tx)?;
            expired.push(loaded.record.frontmatter.id);
        }
        Ok(expired)
    }

    pub fn review(
        &self,
        op: OpId,
        caller: Caller,
        pending: &MemoryId,
        decision: ReviewDecision,
    ) -> Result<Admission, StoreError> {
        if !caller.is_user() {
            return Err(StoreError::ForbiddenActor(
                "only a trusted user review may accept or reject pending memory".into(),
            ));
        }
        let _lock = self.lock_exclusive_recovered()?;
        let digest = request_digest(&(&caller, pending, &decision))?;
        if let Some(admission) = self.replay_intent(&op, &digest)? {
            return Ok(admission);
        }
        let record = self.load_pending(pending)?;
        match decision {
            ReviewDecision::Reject { reason } => {
                self.reject_pending_locked(op, caller, record, reason, digest)
            }
            ReviewDecision::Accept => {
                let op_for_replay = op.clone();
                let digest_for_replay = digest.clone();
                match self.accept_pending_locked(op, &caller, record, digest) {
                    Ok(admission) => Ok(admission),
                    Err(StoreError::Conflict {
                        target,
                        current,
                        expected,
                    }) => {
                        let admission = Admission::Conflict {
                            target,
                            current,
                            expected,
                        };
                        self.finish_noop(&op_for_replay, &digest_for_replay, admission.clone())?;
                        Ok(admission)
                    }
                    Err(error) => Err(error),
                }
            }
        }
    }

    pub fn recover(&self) -> Result<RecoveryReport, StoreError> {
        let _lock = self.lock_exclusive()?;
        let mut report = self.recover_locked()?;
        report.cleaned = self.cleanup_orphans()?;
        Ok(report)
    }

    /// Inspect a scope without creating, recovering, or writing anything.
    ///
    /// Unlike every other entry point this never finishes a prepared
    /// transaction and never repairs a file: what a person needs to know is
    /// reported, not applied.
    pub fn diagnose(roots: ScopeRoots) -> Result<DiagnosticReport, StoreError> {
        let paths = StorePaths::inspect(&roots)?;
        let store = Self {
            paths,
            scope: roots.scope,
        };
        let _lock = if store.paths.lock_path.is_file() {
            Some(store.lock_shared()?)
        } else {
            None
        };
        store.diagnose_locked()
    }

    fn diagnose_locked(&self) -> Result<DiagnosticReport, StoreError> {
        let mut report = DiagnosticReport {
            scope: self.scope,
            current_root: self.paths.current_root.clone(),
            state_root: self.paths.state_root.clone(),
            initialized: self.paths.current_root.is_dir() && self.paths.state_root.is_dir(),
            active_records: 0,
            pending_records: 0,
            issues: Vec::new(),
        };

        for path in path::list_json(&self.paths.journal_dir)? {
            match JournalPlan::load(&path) {
                Ok(plan) => match plan.state {
                    JournalState::Blocked => report.issues.push(DiagnosticIssue {
                        severity: DiagnosticSeverity::Blocking,
                        kind: DiagnosticKind::BlockedJournal,
                        path: Some(path),
                        detail: format!(
                            "operation {} is blocked: {}",
                            plan.op_id,
                            plan.blocked_reason.as_deref().unwrap_or("no reason recorded")
                        ),
                        suggestion: "Inspect the files named by the journal and resolve the conflict by hand; recovery refuses to guess a third state.".into(),
                    }),
                    JournalState::Prepared | JournalState::Committed => {
                        report.issues.push(DiagnosticIssue {
                            severity: DiagnosticSeverity::Warning,
                            kind: DiagnosticKind::PendingJournal,
                            path: Some(path),
                            detail: format!(
                                "operation {} awaits recovery ({:?})",
                                plan.op_id, plan.state
                            ),
                            suggestion: "The next read or write by any process finishes this transaction; no manual action is needed.".into(),
                        })
                    }
                },
                Err(error) => report.issues.push(DiagnosticIssue {
                    severity: DiagnosticSeverity::Blocking,
                    kind: DiagnosticKind::InvalidJournal,
                    path: Some(path),
                    detail: format!("cannot read journal: {error}"),
                    suggestion: "Keep the file for inspection and restore a valid journal, or move it aside only after checking the current root by hand.".into(),
                }),
            }
        }

        let mut subjects: Vec<(MemoryKind, String, MemoryId)> = Vec::new();
        for path in path::list_markdown(&self.paths.current_root)? {
            let (record, bytes) = match path::read_bytes(&path) {
                Ok(bytes) => match frontmatter::parse(&bytes) {
                    Ok(record) => (record, bytes),
                    Err(error) => {
                        report.issues.push(DiagnosticIssue {
                            severity: DiagnosticSeverity::Blocking,
                            kind: DiagnosticKind::InvalidCurrentDocument,
                            path: Some(path),
                            detail: format!("current document is invalid: {error}"),
                            suggestion: "Restore the file from its history revision or move it out of the current root; the store will not auto-repair it.".into(),
                        });
                        continue;
                    }
                },
                Err(error) => {
                    report.issues.push(DiagnosticIssue {
                        severity: DiagnosticSeverity::Blocking,
                        kind: DiagnosticKind::InvalidCurrentDocument,
                        path: Some(path),
                        detail: format!("current document cannot be read: {error}"),
                        suggestion: "Check the file's permissions and content; the store will not skip it and report an empty scope.".into(),
                    });
                    continue;
                }
            };
            if record.frontmatter.status != MemoryStatus::Active {
                report.issues.push(DiagnosticIssue {
                    severity: DiagnosticSeverity::Blocking,
                    kind: DiagnosticKind::NonActiveCurrentRecord,
                    path: Some(path),
                    detail: format!(
                        "current root holds a {:?} record ({})",
                        record.frontmatter.status, record.frontmatter.id
                    ),
                    suggestion: "Move the record back to pending or history; a current root may only hold active records.".into(),
                });
                continue;
            }
            if record.frontmatter.scope != self.scope {
                report.issues.push(DiagnosticIssue {
                    severity: DiagnosticSeverity::Blocking,
                    kind: DiagnosticKind::ScopeMismatch,
                    path: Some(path),
                    detail: format!(
                        "record {} belongs to {:?} but this store is {:?}",
                        record.frontmatter.id, record.frontmatter.scope, self.scope
                    ),
                    suggestion: "Move the record to the store for its own scope.".into(),
                });
                continue;
            }
            let digest = frontmatter::sha256_hex(&bytes);
            let receipt = match self.load_receipt(&record.frontmatter.id) {
                Ok(receipt) => receipt,
                Err(error) => {
                    report.issues.push(DiagnosticIssue {
                        severity: DiagnosticSeverity::Blocking,
                        kind: DiagnosticKind::InvalidReceipt,
                        path: Some(self.paths.receipt_file(&record.frontmatter.id)),
                        detail: format!(
                            "receipt for {} cannot be read: {error}",
                            record.frontmatter.id
                        ),
                        suggestion: "Restore the receipt from history metadata or re-record the fact through the store; without it this file is not trusted.".into(),
                    });
                    continue;
                }
            };
            let trust = receipt::classify(&record, &digest, receipt.as_ref());
            match trust.trust {
                Trust::ExternalConflict => {
                    report.issues.push(DiagnosticIssue {
                        severity: DiagnosticSeverity::ActionRequired,
                        kind: DiagnosticKind::ExternalConflict,
                        path: Some(path),
                        detail: format!(
                            "record {} no longer matches the revision its receipt committed",
                            record.frontmatter.id
                        ),
                        suggestion: "Review the external change, then re-record it through the store or restore the receipted revision from history.".into(),
                    });
                    continue;
                }
                Trust::ExternalUnverified => {
                    report.issues.push(DiagnosticIssue {
                        severity: DiagnosticSeverity::ActionRequired,
                        kind: DiagnosticKind::ExternalUnverified,
                        path: Some(path),
                        detail: format!(
                            "record {} was edited outside the store and is not used as truth",
                            record.frontmatter.id
                        ),
                        suggestion: "Re-record the change through the store, or restore the receipted revision from history.".into(),
                    });
                    continue;
                }
                Trust::Trusted => {}
            }
            report.active_records += 1;
            if record.frontmatter.kind == MemoryKind::UserFact
                && let Some(subject_key) = record.frontmatter.subject_key.clone()
            {
                subjects.push((
                    record.frontmatter.kind,
                    subject_key,
                    record.frontmatter.id.clone(),
                ));
            }
        }

        for (index, (kind, subject_key, _)) in subjects.iter().enumerate() {
            if subjects[..index]
                .iter()
                .any(|(seen_kind, seen_key, _)| seen_kind == kind && seen_key == subject_key)
            {
                continue;
            }
            let ids: Vec<&MemoryId> = subjects
                .iter()
                .filter(|(other_kind, other_key, _)| other_kind == kind && other_key == subject_key)
                .map(|(_, _, id)| id)
                .collect();
            if ids.len() <= 1 {
                continue;
            }
            report.issues.push(DiagnosticIssue {
                severity: DiagnosticSeverity::Blocking,
                kind: DiagnosticKind::DuplicateActiveSubject,
                path: None,
                detail: format!(
                    "{:?} records {ids:?} share subject_key {subject_key}",
                    ids.len()
                ),
                suggestion: "Keep one record current and move the others out of the current root; the store will not pick a winner.".into(),
            });
        }

        for path in path::list_markdown(&self.paths.pending_dir)? {
            let parsed = match path::read_bytes(&path) {
                Ok(bytes) => frontmatter::parse(&bytes),
                Err(error) => Err(DocumentError::Invalid(error.to_string())),
            };
            match parsed {
                Ok(record)
                    if record.frontmatter.status == MemoryStatus::Pending
                        && record.frontmatter.proposal.is_some() =>
                {
                    report.pending_records += 1;
                }
                Ok(record) => report.issues.push(DiagnosticIssue {
                    severity: DiagnosticSeverity::ActionRequired,
                    kind: DiagnosticKind::InvalidPendingDocument,
                    path: Some(path),
                    detail: format!(
                        "pending document {} has status {:?} and proposal={}",
                        record.frontmatter.id,
                        record.frontmatter.status,
                        record.frontmatter.proposal.is_some()
                    ),
                    suggestion: "Accept or reject the proposal through a user review; the file is not auto-activated.".into(),
                }),
                Err(error) => report.issues.push(DiagnosticIssue {
                    severity: DiagnosticSeverity::ActionRequired,
                    kind: DiagnosticKind::InvalidPendingDocument,
                    path: Some(path),
                    detail: format!("pending document is invalid: {error}"),
                    suggestion: "Keep the file for inspection; rejecting it through the store is the only supported removal.".into(),
                }),
            }
        }

        for path in path::list_regular_files(&self.paths.stage_dir)? {
            report.issues.push(DiagnosticIssue {
                severity: DiagnosticSeverity::Warning,
                kind: DiagnosticKind::OrphanStageFile,
                path: Some(path),
                detail: "an uncommitted staging file remains".into(),
                suggestion:
                    "No action: the next recovery removes it, and it was never current truth."
                        .into(),
            });
        }

        report.issues.sort_by(|left, right| {
            left.severity
                .cmp(&right.severity)
                .then_with(|| left.path.cmp(&right.path))
                .then_with(|| left.detail.cmp(&right.detail))
        });
        Ok(report)
    }

    // ----- lock and recovery -------------------------------------------------

    fn lock_shared(&self) -> Result<ScopeLock, StoreError> {
        ScopeLock::acquire(&self.paths.lock_path, false).map_err(Into::into)
    }

    fn lock_exclusive(&self) -> Result<ScopeLock, StoreError> {
        ScopeLock::acquire(&self.paths.lock_path, true).map_err(Into::into)
    }

    fn lock_shared_recovered(&self) -> Result<ScopeLock, StoreError> {
        loop {
            let guard = self.lock_shared()?;
            match self.journal_condition()? {
                JournalCondition::Clean => return Ok(guard),
                JournalCondition::Blocked => {
                    return Err(StoreError::RecoveryConflict {
                        op_id: None,
                        reason: "a blocked journal must be resolved before reading".into(),
                    });
                }
                JournalCondition::Prepared => {
                    drop(guard);
                    let report = self.recover_exclusive()?;
                    if report.is_blocked() {
                        return Err(StoreError::RecoveryConflict {
                            op_id: report.blocked.first().cloned(),
                            reason: "journal recovery left a blocked transaction".into(),
                        });
                    }
                }
            }
        }
    }

    fn lock_exclusive_recovered(&self) -> Result<ScopeLock, StoreError> {
        let guard = self.lock_exclusive()?;
        let report = self.recover_locked()?;
        if let Some(op_id) = report.blocked.first() {
            return Err(StoreError::RecoveryConflict {
                op_id: Some(op_id.clone()),
                reason: "a blocked journal must be resolved before writing".into(),
            });
        }
        Ok(guard)
    }

    fn recover_exclusive(&self) -> Result<RecoveryReport, StoreError> {
        let _lock = self.lock_exclusive()?;
        self.recover_locked()
    }

    fn journal_condition(&self) -> Result<JournalCondition, StoreError> {
        let mut prepared = false;
        for path in path::list_json(&self.paths.journal_dir)? {
            let plan = JournalPlan::load(&path).map_err(|error| StoreError::RecoveryConflict {
                op_id: None,
                reason: format!("cannot read journal {}: {error}", path.display()),
            })?;
            match plan.state {
                JournalState::Blocked => return Ok(JournalCondition::Blocked),
                JournalState::Prepared => prepared = true,
                JournalState::Committed => {}
            }
        }
        Ok(if prepared {
            JournalCondition::Prepared
        } else {
            JournalCondition::Clean
        })
    }

    fn recover_locked(&self) -> Result<RecoveryReport, StoreError> {
        StoreLifecycleMetrics::count(&STORE_METRICS.journal_recoveries);
        let mut report = RecoveryReport::default();
        let journal_paths = path::list_json(&self.paths.journal_dir)?;
        let mut parsed = Vec::new();
        for journal_path in &journal_paths {
            let plan =
                JournalPlan::load(journal_path).map_err(|error| StoreError::RecoveryConflict {
                    op_id: None,
                    reason: format!("cannot read journal {}: {error}", journal_path.display()),
                })?;
            self.validate_plan_paths(&plan, journal_path)?;
            parsed.push((journal_path.clone(), plan));
        }
        for (journal_path, mut plan) in parsed {
            match plan.state {
                JournalState::Blocked => report.blocked.push(plan.op_id),
                JournalState::Prepared | JournalState::Committed => {
                    match self.finish_plan(&mut plan, &journal_path) {
                        Ok(()) => report.recovered.push(plan.op_id),
                        Err(StoreError::RecoveryConflict { reason, .. })
                        | Err(StoreError::ExternalConflict { reason, .. }) => {
                            let _ = plan.mark_blocked(&journal_path, reason);
                            report.blocked.push(plan.op_id);
                        }
                        Err(error) => return Err(error),
                    }
                }
            }
        }
        report.cleaned = Vec::new();
        Ok(report)
    }

    fn validate_plan_paths(
        &self,
        plan: &JournalPlan,
        journal_path: &Path,
    ) -> Result<(), StoreError> {
        if plan.schema != super::journal::JOURNAL_SCHEMA {
            return Err(StoreError::RecoveryConflict {
                op_id: Some(plan.op_id.clone()),
                reason: "unsupported journal schema".into(),
            });
        }
        let expected_journal = self.paths.journal_file(&plan.op_id);
        if expected_journal != journal_path {
            return Err(StoreError::RecoveryConflict {
                op_id: Some(plan.op_id.clone()),
                reason: "journal filename does not match its operation".into(),
            });
        }
        for file in &plan.new_files {
            if file.final_path != self.paths.current_file(&file.id) {
                return Err(self.invalid_journal(plan, "new file target is outside current root"));
            }
            if file.staged_path.parent() != Some(self.paths.stage_dir.as_path()) {
                return Err(self.invalid_journal(plan, "staged file is outside the stage dir"));
            }
        }
        for file in &plan.state_files {
            if !path::is_inside(&file.final_path, &self.paths.state_root) {
                return Err(self.invalid_journal(plan, "state file target is outside state root"));
            }
            if file.temp_path.parent() != file.final_path.parent() {
                return Err(self.invalid_journal(plan, "state temp is not beside its target"));
            }
            let expected = match &file.kind {
                StateFileKind::History { id, revision } => self.paths.history_file(id, *revision),
                StateFileKind::Receipt => self.paths.receipt_file(&plan_id(plan)),
                StateFileKind::Intent => self.paths.intent_file(&plan.op_id),
                StateFileKind::Pending { id } => self.paths.pending_file(id),
                StateFileKind::Attic { id } => self.paths.attic_file(id),
                StateFileKind::Tombstone { id } => self.paths.tombstone_file(id),
            };
            if expected != file.final_path {
                return Err(self.invalid_journal(plan, "state file target is not canonical"));
            }
        }
        for dir in &plan.delete_dirs {
            if !path::is_inside(dir, &self.paths.state_root) {
                return Err(self.invalid_journal(plan, "delete directory is outside state root"));
            }
        }
        for append in &plan.appends {
            if append.path != self.paths.suppressed_path
                && append.path != self.paths.pending_history_path
            {
                return Err(self.invalid_journal(plan, "append target is not a managed file"));
            }
        }
        Ok(())
    }

    fn invalid_journal(&self, plan: &JournalPlan, reason: &str) -> StoreError {
        StoreError::RecoveryConflict {
            op_id: Some(plan.op_id.clone()),
            reason: reason.into(),
        }
    }

    // ----- admission ---------------------------------------------------------

    fn admit_locked(&self, request: AdmitRequest) -> Result<Admission, StoreError> {
        let digest = request_digest(&(&request.caller, &request.intent, &request.expected))?;
        self.admit_locked_with_digest(request, digest)
    }

    fn admit_locked_with_digest(
        &self,
        request: AdmitRequest,
        digest: String,
    ) -> Result<Admission, StoreError> {
        self.admit_locked_with_digest_mode(request, digest, None)
    }

    fn admit_locked_with_digest_mode(
        &self,
        request: AdmitRequest,
        digest: String,
        forced_pending: Option<super::lifecycle::PendingReason>,
    ) -> Result<Admission, StoreError> {
        if let Some(admission) = self.replay_intent(&request.op_id, &digest)? {
            return Ok(admission);
        }
        let caller = request.caller;
        let intent = request.intent;
        if caller.is_external() {
            let admission = Admission::Rejected {
                reason: RejectReason::ExternalCaller,
            };
            self.finish_noop(&request.op_id, &digest, admission.clone())?;
            return Ok(admission);
        }
        if intent.scope != self.scope {
            let admission = Admission::Rejected {
                reason: RejectReason::ScopeMismatch,
            };
            self.finish_noop(&request.op_id, &digest, admission.clone())?;
            return Ok(admission);
        }
        // A keyless user fact is the user's own unstructured memory: its
        // identity is the record id, never an invented subject key. Only a
        // trusted user operation (or a migration of such a record) may create
        // one; an automatic writer must supply a structured subject. An
        // automatic UPDATE keeps the target's keyless identity instead of
        // inventing one: it is parked for user review by the lifecycle.
        if intent.kind == MemoryKind::UserFact
            && intent.subject_key.is_none()
            && !caller.is_user()
            && !matches!(caller, Caller::Migration { .. })
            && !matches!(intent.relation, Relation::Update { .. })
        {
            let admission = Admission::Rejected {
                reason: RejectReason::MissingSubjectKey,
            };
            self.finish_noop(&request.op_id, &digest, admission.clone())?;
            return Ok(admission);
        }
        // `updated_at` is set at admission time, so a TTL that has already
        // elapsed would render a document the reader rejects as
        // `ExpiredButActive`. Refuse it instead of writing an unreadable file.
        if intent
            .expires_at
            .is_some_and(|expires| expires <= chrono::Utc::now())
        {
            let admission = Admission::Rejected {
                reason: RejectReason::InvalidIntent,
            };
            self.finish_noop(&request.op_id, &digest, admission.clone())?;
            return Ok(admission);
        }
        if !caller.is_user() {
            let fingerprint = suppression_fingerprint(
                intent.kind,
                intent.scope,
                intent.subject_key.as_deref(),
                intent.value.as_deref(),
                &intent.body,
                relation_operation(&intent.relation),
                &relation_targets(&intent.relation),
            )?;
            if self.is_suppressed(&fingerprint)? {
                let admission = Admission::Rejected {
                    reason: RejectReason::Other("rejected_previously".into()),
                };
                self.finish_noop(&request.op_id, &digest, admission.clone())?;
                return Ok(admission);
            }
        }
        self.admit_relation(
            request.op_id,
            caller,
            intent,
            request.expected,
            digest,
            forced_pending,
        )
    }

    fn admit_relation(
        &self,
        op_id: OpId,
        caller: Caller,
        intent: MemoryIntent,
        expected: Option<Cas>,
        digest: String,
        forced_pending: Option<super::lifecycle::PendingReason>,
    ) -> Result<Admission, StoreError> {
        if forced_pending.is_some() && !matches!(intent.relation, Relation::None) {
            return Err(StoreError::Invalid(
                "only a create can be parked as a pending proposal".into(),
            ));
        }
        let now = Utc::now();
        let new_authority = creation_authority(&caller, intent.kind, &intent);
        match intent.relation.clone() {
            Relation::None => self.admit_create(
                op_id,
                caller,
                intent,
                new_authority,
                expected,
                digest,
                now,
                forced_pending,
            ),
            Relation::Update { target } => self.admit_update(
                op_id,
                caller,
                intent,
                target,
                new_authority,
                expected,
                digest,
                now,
            ),
            Relation::Supersede { targets } => self.admit_supersede(
                op_id,
                caller,
                intent,
                targets,
                new_authority,
                expected,
                digest,
                now,
            ),
            Relation::Replaces { target } => self.admit_supersede(
                op_id,
                caller,
                intent,
                vec![target],
                new_authority,
                expected,
                digest,
                now,
            ),
        }
    }

    fn admit_create(
        &self,
        op_id: OpId,
        caller: Caller,
        intent: MemoryIntent,
        new_authority: MemoryAuthority,
        _expected: Option<Cas>,
        digest: String,
        now: DateTime<Utc>,
        forced_pending: Option<super::lifecycle::PendingReason>,
    ) -> Result<Admission, StoreError> {
        let creation = match forced_pending {
            Some(reason) => CreationDecision::Pending(reason),
            None => lifecycle::creation_decision(
                &caller,
                intent.kind,
                intent.scope,
                intent.experience.as_ref(),
                lifecycle::user_statement_is_proven(&intent),
            ),
        };
        match creation {
            CreationDecision::Rejected(reason) => {
                let admission = Admission::Rejected { reason };
                self.finish_noop(&op_id, &digest, admission.clone())?;
                Ok(admission)
            }
            CreationDecision::Pending(reason) => {
                let id = MemoryId::new();
                let proposal = Proposal {
                    operation: Operation::Create,
                    target_ids: Vec::new(),
                    expected_revision: None,
                    expected_digest: None,
                    proposed_authority: Some(intent.proposed_authority),
                    reason: reason.as_str().to_string(),
                };
                let record = self.build_record(
                    &intent,
                    id.clone(),
                    1,
                    MemoryStatus::Pending,
                    new_authority,
                    now,
                    now,
                    Some(proposal),
                    Vec::new(),
                )?;
                let bytes = frontmatter::render(&record)?;
                let admission = Admission::Pending {
                    id: id.clone(),
                    reason,
                };
                self.commit_pending(op_id, caller, digest, record, bytes, admission)
            }
            CreationDecision::Active(authority) => {
                // A user fact that restates a span already owned by a trusted
                // direct user write is not a second fact. The match is exact
                // provenance (the same user-turn span), never text similarity,
                // and it only covers that one operation: other spans from the
                // same turn still flow through normal admission.
                if intent.kind == MemoryKind::UserFact
                    && let Some(owner) = self.direct_owner_for(&caller, &intent)?
                {
                    let existing_id = owner.record.frontmatter.id.clone();
                    let admission = Admission::Skipped {
                        existing: existing_id.clone(),
                        reason: SkipReason::DirectCommandOwned,
                    };
                    return self.finish_skip(op_id, &digest, admission, &existing_id);
                }
                if intent.kind == MemoryKind::UserFact && intent.subject_key.is_none() {
                    // Unstructured user memory is addressable only by its exact
                    // id; it never participates in subject-key identity.
                    // Automatic writers were rejected above.
                    return self.commit_new_with_targets(
                        op_id,
                        caller,
                        digest,
                        intent,
                        authority,
                        Vec::new(),
                        Operation::Create,
                        now,
                    );
                }
                {
                    // One identity has one current record, per kind: a lesson
                    // whose extractor-derived identity matches a current lesson
                    // updates it instead of stacking a second copy. The
                    // comparison is the structured subject key the producer
                    // supplied, never text similarity.
                    let Some(subject_key) = intent.subject_key.clone() else {
                        return self.commit_new_with_targets(
                            op_id,
                            caller,
                            digest,
                            intent,
                            authority,
                            Vec::new(),
                            Operation::Create,
                            now,
                        );
                    };
                    let existing = self.active_by_subject(intent.kind, &subject_key, now)?;
                    if existing.len() > 1 {
                        return Err(StoreError::InvariantViolation {
                            subject_key,
                            ids: existing
                                .iter()
                                .map(|loaded| loaded.record.frontmatter.id.clone())
                                .collect(),
                        });
                    }
                    if let Some(target) = existing.first() {
                        if lifecycle::is_identical(&target.record, &intent) {
                            let existing_id = target.record.frontmatter.id.clone();
                            let admission = Admission::Skipped {
                                existing: existing_id.clone(),
                                reason: SkipReason::Identical,
                            };
                            return self.finish_skip(op_id, &digest, admission, &existing_id);
                        }
                        let has_new_evidence = lifecycle::has_new_evidence(&target.record, &intent);
                        match lifecycle::decide_target(
                            &caller,
                            &target.view(now),
                            new_authority,
                            has_new_evidence,
                        ) {
                            TargetDecision::Replace => {
                                return self.commit_new_with_targets(
                                    op_id,
                                    caller,
                                    digest,
                                    intent,
                                    authority,
                                    vec![TargetAction {
                                        loaded: target.clone(),
                                        disposition: TargetDisposition::Retire,
                                        final_status: Some(MemoryStatus::Superseded),
                                        superseded_by: None,
                                        reason: None,
                                    }],
                                    Operation::Supersede,
                                    now,
                                );
                            }
                            TargetDecision::Pending(reason) => {
                                return self.commit_identity_pending(
                                    op_id, caller, digest, intent, authority, target, reason, now,
                                );
                            }
                        }
                    }
                }
                self.commit_new_with_targets(
                    op_id,
                    caller,
                    digest,
                    intent,
                    authority,
                    Vec::new(),
                    Operation::Create,
                    now,
                )
            }
        }
    }

    fn admit_update(
        &self,
        op_id: OpId,
        caller: Caller,
        intent: MemoryIntent,
        target_id: MemoryId,
        new_authority: MemoryAuthority,
        expected: Option<Cas>,
        digest: String,
        now: DateTime<Utc>,
    ) -> Result<Admission, StoreError> {
        let expected = expected.ok_or_else(|| {
            StoreError::Invalid("update requires an expected revision and digest".into())
        })?;
        let Some(target) = self.load_current(&target_id)? else {
            let admission = Admission::Rejected {
                reason: RejectReason::Other("target_not_active".into()),
            };
            self.finish_noop(&op_id, &digest, admission.clone())?;
            return Ok(admission);
        };
        if target.cas() != expected {
            let admission = Admission::Conflict {
                target: target_id,
                current: target.cas(),
                expected,
            };
            self.finish_noop(&op_id, &digest, admission.clone())?;
            return Ok(admission);
        }
        if target.trust.trust != Trust::Trusted {
            return Err(StoreError::ExternalConflict {
                id: target.record.frontmatter.id.clone(),
                reason: "target is not a trusted current record".into(),
            });
        }
        if target.record.frontmatter.kind != intent.kind {
            let admission = Admission::Rejected {
                reason: RejectReason::InvalidIntent,
            };
            self.finish_noop(&op_id, &digest, admission.clone())?;
            return Ok(admission);
        }
        // An explicit record may gain a structured identity, by exact id, but
        // an existing identity is never cleared or swapped through an update:
        // that would leave the old identity's history unexplained. Use an
        // explicit supersede instead.
        let identity_added =
            target.record.frontmatter.subject_key.is_none() && intent.subject_key.is_some();
        if !identity_added && target.record.frontmatter.subject_key != intent.subject_key {
            let admission = Admission::Rejected {
                reason: RejectReason::InvalidIntent,
            };
            self.finish_noop(&op_id, &digest, admission.clone())?;
            return Ok(admission);
        }
        let mut forced_pending = None;
        if identity_added {
            if !caller.is_user() {
                // The target is the user's unstructured memory. A model or
                // runtime may suggest an identity, but a person must decide.
                forced_pending = Some(PendingReason::ProtectedUserFact);
            } else {
                let subject_key = intent.subject_key.as_deref().unwrap_or_default();
                let collisions = self.active_by_subject(intent.kind, subject_key, now)?;
                if collisions.len() > 1 {
                    return Err(StoreError::InvariantViolation {
                        subject_key: subject_key.to_string(),
                        ids: collisions
                            .iter()
                            .map(|loaded| loaded.record.frontmatter.id.clone())
                            .collect(),
                    });
                }
                if let Some(collision) = collisions.first() {
                    // Two Actives under one subject are never created by an
                    // update. The person reviews the conversion first.
                    forced_pending = Some(PendingReason::Other(format!(
                        "subject_key_in_use:{}",
                        collision.record.frontmatter.id
                    )));
                }
            }
        }
        let has_new_evidence = lifecycle::has_new_evidence(&target.record, &intent);
        let decision = match forced_pending {
            Some(reason) => TargetDecision::Pending(reason),
            None => lifecycle::decide_target(
                &caller,
                &target.view(now),
                new_authority,
                has_new_evidence,
            ),
        };
        match decision {
            TargetDecision::Pending(reason) => {
                let proposal = Proposal {
                    operation: Operation::Update,
                    target_ids: vec![target_id.to_string()],
                    expected_revision: Some(target.record.frontmatter.revision),
                    expected_digest: Some(target.digest.clone()),
                    proposed_authority: Some(intent.proposed_authority),
                    reason: reason.as_str().to_string(),
                };
                let record = self.build_record(
                    &intent,
                    target_id.clone(),
                    target.record.frontmatter.revision,
                    MemoryStatus::Pending,
                    new_authority,
                    target.record.frontmatter.created_at,
                    now,
                    Some(proposal),
                    Vec::new(),
                )?;
                let bytes = frontmatter::render(&record)?;
                let admission = Admission::Pending {
                    id: target_id,
                    reason,
                };
                self.commit_pending(op_id, caller, digest, record, bytes, admission)
            }
            TargetDecision::Replace => {
                let record = self.build_record(
                    &intent,
                    target_id.clone(),
                    target.record.frontmatter.revision + 1,
                    MemoryStatus::Active,
                    new_authority,
                    target.record.frontmatter.created_at,
                    now,
                    None,
                    Vec::new(),
                )?;
                let bytes = frontmatter::render(&record)?;
                let receipt = Receipt::from_record(&record, &frontmatter::sha256_hex(&bytes));
                let admission = Admission::Applied {
                    id: target_id.clone(),
                    revision: record.frontmatter.revision,
                    operation: Operation::Update,
                    possible_duplicate: Vec::new(),
                };
                let mut tx = self.tx_with_intent(
                    Operation::Update,
                    caller.label(),
                    op_id,
                    digest,
                    admission.clone(),
                );
                tx.new_record = Some((record, bytes));
                tx.receipt = Some(receipt);
                tx.targets.push(TargetAction {
                    loaded: target,
                    disposition: TargetDisposition::Overwritten,
                    final_status: None,
                    superseded_by: None,
                    reason: None,
                });
                self.execute_tx(tx)?;
                Ok(admission)
            }
        }
    }

    fn admit_supersede(
        &self,
        op_id: OpId,
        caller: Caller,
        intent: MemoryIntent,
        targets: Vec<MemoryId>,
        new_authority: MemoryAuthority,
        expected: Option<Cas>,
        digest: String,
        now: DateTime<Utc>,
    ) -> Result<Admission, StoreError> {
        if targets.is_empty() {
            let admission = Admission::Rejected {
                reason: RejectReason::InvalidIntent,
            };
            self.finish_noop(&op_id, &digest, admission.clone())?;
            return Ok(admission);
        }
        if expected.is_some() && targets.len() != 1 {
            let admission = Admission::Rejected {
                reason: RejectReason::InvalidIntent,
            };
            self.finish_noop(&op_id, &digest, admission.clone())?;
            return Ok(admission);
        }
        let mut loaded_targets = Vec::new();
        for target_id in &targets {
            let Some(target) = self.load_current(target_id)? else {
                let admission = Admission::Rejected {
                    reason: RejectReason::Other("target_not_active".into()),
                };
                self.finish_noop(&op_id, &digest, admission.clone())?;
                return Ok(admission);
            };
            if let Some(expected) = &expected
                && target.cas() != *expected
            {
                let admission = Admission::Conflict {
                    target: target_id.clone(),
                    current: target.cas(),
                    expected: expected.clone(),
                };
                self.finish_noop(&op_id, &digest, admission.clone())?;
                return Ok(admission);
            }
            if target.trust.trust != Trust::Trusted {
                return Err(StoreError::ExternalConflict {
                    id: target_id.clone(),
                    reason: "target is not a trusted current record".into(),
                });
            }
            loaded_targets.push(target);
        }
        let mut pending_reason = None;
        for target in &loaded_targets {
            let has_new_evidence = lifecycle::has_new_evidence(&target.record, &intent);
            match lifecycle::decide_target(
                &caller,
                &target.view(now),
                new_authority,
                has_new_evidence,
            ) {
                TargetDecision::Replace => {}
                TargetDecision::Pending(reason) => pending_reason = Some(reason),
            }
        }
        if let Some(reason) = pending_reason {
            let id = MemoryId::new();
            let proposal = Proposal {
                operation: Operation::Supersede,
                target_ids: targets.iter().map(ToString::to_string).collect(),
                expected_revision: targets
                    .first()
                    .and_then(|_| loaded_targets.first())
                    .map(|target| target.record.frontmatter.revision),
                expected_digest: loaded_targets.first().map(|target| target.digest.clone()),
                proposed_authority: Some(intent.proposed_authority),
                reason: reason.as_str().to_string(),
            };
            let record = self.build_record(
                &intent,
                id.clone(),
                1,
                MemoryStatus::Pending,
                new_authority,
                now,
                now,
                Some(proposal),
                targets.iter().map(ToString::to_string).collect(),
            )?;
            let bytes = frontmatter::render(&record)?;
            let admission = Admission::Pending { id, reason };
            return self.commit_pending(op_id, caller, digest, record, bytes, admission);
        }
        // `commit_new_with_targets` allocates the replacement id and fills
        // `superseded_by` itself; allocating one here would leave history
        // pointing at an id that never becomes current.
        let target_actions = loaded_targets
            .into_iter()
            .map(|loaded| TargetAction {
                loaded,
                disposition: TargetDisposition::Retire,
                final_status: Some(MemoryStatus::Superseded),
                superseded_by: None,
                reason: None,
            })
            .collect();
        self.commit_new_with_targets(
            op_id,
            caller,
            digest,
            intent,
            new_authority,
            target_actions,
            Operation::Supersede,
            now,
        )
    }

    fn commit_identity_pending(
        &self,
        op_id: OpId,
        caller: Caller,
        digest: String,
        intent: MemoryIntent,
        authority: MemoryAuthority,
        target: &Loaded,
        reason: PendingReason,
        now: DateTime<Utc>,
    ) -> Result<Admission, StoreError> {
        let id = MemoryId::new();
        let proposal = Proposal {
            operation: Operation::Supersede,
            target_ids: vec![target.record.frontmatter.id.to_string()],
            expected_revision: Some(target.record.frontmatter.revision),
            expected_digest: Some(target.digest.clone()),
            proposed_authority: Some(intent.proposed_authority),
            reason: reason.as_str().to_string(),
        };
        let record = self.build_record(
            &intent,
            id.clone(),
            1,
            MemoryStatus::Pending,
            authority,
            now,
            now,
            Some(proposal),
            vec![target.record.frontmatter.id.to_string()],
        )?;
        let bytes = frontmatter::render(&record)?;
        let admission = Admission::Pending { id, reason };
        self.commit_pending(op_id, caller, digest, record, bytes, admission)
    }

    #[allow(clippy::too_many_arguments)]
    fn commit_new_with_targets(
        &self,
        op_id: OpId,
        caller: Caller,
        digest: String,
        intent: MemoryIntent,
        authority: MemoryAuthority,
        mut targets: Vec<TargetAction>,
        operation: Operation,
        now: DateTime<Utc>,
    ) -> Result<Admission, StoreError> {
        let id = MemoryId::new();
        let supersedes = targets
            .iter()
            .map(|target| target.loaded.record.frontmatter.id.to_string())
            .collect::<Vec<_>>();
        let record = self.build_record(
            &intent,
            id.clone(),
            1,
            MemoryStatus::Active,
            authority,
            now,
            now,
            None,
            supersedes,
        )?;
        let bytes = frontmatter::render(&record)?;
        let possible_duplicate =
            self.possible_duplicates(&id, intent.kind, &intent.body, &targets)?;
        for target in &mut targets {
            if target.superseded_by.is_none() && target.disposition == TargetDisposition::Retire {
                target.superseded_by = Some(id.clone());
            }
        }
        let admission = Admission::Applied {
            id: id.clone(),
            revision: 1,
            operation,
            possible_duplicate,
        };
        let mut tx = self.tx_with_intent(
            operation,
            caller.label(),
            op_id.clone(),
            digest,
            admission.clone(),
        );
        tx.new_record = Some((record.clone(), bytes.clone()));
        tx.receipt = Some(Receipt::from_record(
            &record,
            &frontmatter::sha256_hex(&bytes),
        ));
        tx.targets = targets.clone();
        for target in &targets {
            if let Some(status) = target.final_status {
                tx.attic.push(attic_entry(
                    &target.loaded,
                    status,
                    target.superseded_by.clone(),
                    target.reason.clone(),
                ));
            }
        }
        self.add_covered_pending(&record, &op_id, &mut tx)?;
        self.execute_tx(tx)?;
        Ok(admission)
    }

    // ----- destructive and review -------------------------------------------

    fn destructive_locked(
        &self,
        op: OpId,
        caller: Caller,
        id: &MemoryId,
        expected: Cas,
        action: DestructiveAction,
    ) -> Result<Admission, StoreError> {
        let digest = request_digest(&(&caller, id, &expected, &action))?;
        if let Some(admission) = self.replay_intent(&op, &digest)? {
            return Ok(admission);
        }
        if caller.is_external() {
            let admission = Admission::Rejected {
                reason: RejectReason::ExternalCaller,
            };
            self.finish_noop(&op, &digest, admission.clone())?;
            return Ok(admission);
        }
        let Some(target) = self.load_current(id)? else {
            return Err(StoreError::NotFound(id.clone()));
        };
        if target.cas() != expected {
            let admission = Admission::Conflict {
                target: id.clone(),
                current: target.cas(),
                expected,
            };
            self.finish_noop(&op, &digest, admission.clone())?;
            return Ok(admission);
        }
        if target.trust.trust != Trust::Trusted {
            return Err(StoreError::ExternalConflict {
                id: id.clone(),
                reason: "target is not a trusted current record".into(),
            });
        }
        if !caller.is_user() {
            let fingerprint = suppression_fingerprint(
                target.record.frontmatter.kind,
                target.record.frontmatter.scope,
                target.record.frontmatter.subject_key.as_deref(),
                target.record.frontmatter.value.as_deref(),
                &target.record.body,
                action.operation(),
                &[id.to_string()],
            )?;
            if self.is_suppressed(&fingerprint)? {
                let admission = Admission::Rejected {
                    reason: RejectReason::Other("rejected_previously".into()),
                };
                self.finish_noop(&op, &digest, admission.clone())?;
                return Ok(admission);
            }
        }
        let now = Utc::now();
        let new_authority = authority_for_caller(
            &caller,
            target.record.frontmatter.kind,
            target.record.frontmatter.experience.as_ref(),
        );
        let decision = match &action {
            DestructiveAction::Pin { .. } => {
                lifecycle::pin_decision(&caller, &target.view(now), false)
            }
            DestructiveAction::Unpin => {
                if !target.record.frontmatter.pinned {
                    let admission = Admission::Skipped {
                        existing: id.clone(),
                        reason: SkipReason::Identical,
                    };
                    return self.finish_skip(op, &digest, admission, id);
                }
                lifecycle::pin_decision(&caller, &target.view(now), true)
            }
            DestructiveAction::Deprecate { .. } | DestructiveAction::Forget => {
                lifecycle::destructive_decision(&caller, &target.view(now), new_authority)
            }
        };
        match decision {
            TargetDecision::Pending(reason) => {
                let proposal = Proposal {
                    operation: action.operation(),
                    target_ids: vec![id.to_string()],
                    expected_revision: Some(target.record.frontmatter.revision),
                    expected_digest: Some(target.digest.clone()),
                    proposed_authority: Some(new_authority),
                    reason: reason.as_str().to_string(),
                };
                let record = self.build_record_from_record(
                    &target.record,
                    target.record.frontmatter.revision,
                    MemoryStatus::Pending,
                    target.record.frontmatter.authority,
                    now,
                    Some(proposal),
                )?;
                let bytes = frontmatter::render(&record)?;
                let admission = Admission::Pending {
                    id: id.clone(),
                    reason,
                };
                self.commit_pending(op, caller, digest, record, bytes, admission)
            }
            TargetDecision::Replace => match action {
                DestructiveAction::Deprecate { reason } => {
                    let admission = Admission::Applied {
                        id: id.clone(),
                        revision: target.record.frontmatter.revision,
                        operation: Operation::Deprecate,
                        possible_duplicate: Vec::new(),
                    };
                    let mut tx = self.tx_with_intent(
                        Operation::Deprecate,
                        caller.label(),
                        op,
                        digest,
                        admission,
                    );
                    tx.targets.push(TargetAction {
                        loaded: target.clone(),
                        disposition: TargetDisposition::Retire,
                        final_status: Some(MemoryStatus::Deprecated),
                        superseded_by: None,
                        reason: Some(reason.as_str().to_string()),
                    });
                    tx.attic.push(attic_entry(
                        &target,
                        MemoryStatus::Deprecated,
                        None,
                        Some(reason.as_str().to_string()),
                    ));
                    self.execute_tx(tx)?;
                    Ok(Admission::Applied {
                        id: id.clone(),
                        revision: target.record.frontmatter.revision,
                        operation: Operation::Deprecate,
                        possible_duplicate: Vec::new(),
                    })
                }
                DestructiveAction::Forget => {
                    let tombstone = tombstone_for_loaded(&target);
                    let admission = Admission::Applied {
                        id: id.clone(),
                        revision: target.record.frontmatter.revision,
                        operation: Operation::Forget,
                        possible_duplicate: Vec::new(),
                    };
                    let mut tx = self.tx_with_intent(
                        Operation::Forget,
                        caller.label(),
                        op,
                        digest,
                        admission,
                    );
                    tx.targets.push(TargetAction {
                        loaded: target.clone(),
                        disposition: TargetDisposition::Delete,
                        final_status: None,
                        superseded_by: None,
                        reason: None,
                    });
                    tx.tombstones.push(tombstone.clone());
                    tx.delete_dirs
                        .push(self.paths.history_dir_for(&target.record.frontmatter.id));
                    for pending in self.pending_targeting(id)? {
                        tx.pending_deletes.push(pending);
                    }
                    self.execute_tx(tx)?;
                    Ok(Admission::Applied {
                        id: id.clone(),
                        revision: target.record.frontmatter.revision,
                        operation: Operation::Forget,
                        possible_duplicate: Vec::new(),
                    })
                }
                DestructiveAction::Pin { pinned_by } => {
                    let owner = if caller.is_user() {
                        PinOwner::User
                    } else {
                        pinned_by
                    };
                    if target.record.frontmatter.pinned
                        && target.record.frontmatter.pinned_by == Some(owner)
                    {
                        let admission = Admission::Skipped {
                            existing: id.clone(),
                            reason: SkipReason::Identical,
                        };
                        return self.finish_skip(op, &digest, admission, id);
                    }
                    self.pin_commit(op, caller, id, target, owner, true, digest)
                }
                DestructiveAction::Unpin => {
                    self.pin_commit(op, caller, id, target, PinOwner::User, false, digest)
                }
            },
        }
    }

    fn pin_commit(
        &self,
        op: OpId,
        caller: Caller,
        id: &MemoryId,
        target: Loaded,
        owner: PinOwner,
        pinned: bool,
        digest: String,
    ) -> Result<Admission, StoreError> {
        let now = Utc::now();
        let operation = if pinned {
            Operation::Pin
        } else {
            Operation::Unpin
        };
        let mut frontmatter = target.record.frontmatter.clone();
        frontmatter.revision += 1;
        frontmatter.pinned = pinned;
        frontmatter.pinned_by = if pinned { Some(owner) } else { None };
        frontmatter.updated_at = now;
        let record = MemoryRecord {
            frontmatter,
            body: target.record.body.clone(),
        };
        record.validate()?;
        let bytes = frontmatter::render(&record)?;
        let admission = Admission::Applied {
            id: id.clone(),
            revision: record.frontmatter.revision,
            operation,
            possible_duplicate: Vec::new(),
        };
        let mut tx = self.tx_with_intent(operation, caller.label(), op, digest, admission);
        tx.new_record = Some((record.clone(), bytes.clone()));
        tx.receipt = Some(Receipt::from_record(
            &record,
            &frontmatter::sha256_hex(&bytes),
        ));
        tx.targets.push(TargetAction {
            loaded: target,
            disposition: TargetDisposition::Overwritten,
            final_status: None,
            superseded_by: None,
            reason: None,
        });
        self.execute_tx(tx)?;
        Ok(Admission::Applied {
            id: id.clone(),
            revision: record.frontmatter.revision,
            operation,
            possible_duplicate: Vec::new(),
        })
    }

    fn accept_pending_locked(
        &self,
        op: OpId,
        caller: &Caller,
        pending: MemoryRecord,
        digest: String,
    ) -> Result<Admission, StoreError> {
        let pending_id = pending.frontmatter.id.clone();
        let proposal = pending
            .frontmatter
            .proposal
            .clone()
            .ok_or_else(|| StoreError::Invalid("pending record has no proposal".into()))?;
        let now = Utc::now();
        match proposal.operation {
            Operation::Create => {
                let mut record = pending.clone();
                record.frontmatter.status = MemoryStatus::Active;
                record.frontmatter.authority = MemoryAuthority::UserApproved;
                record.frontmatter.proposal = None;
                record.frontmatter.pinned = false;
                record.frontmatter.pinned_by = None;
                record.frontmatter.updated_at = now;
                record.validate()?;
                let bytes = frontmatter::render(&record)?;
                let admission = Admission::Applied {
                    id: pending_id.clone(),
                    revision: record.frontmatter.revision,
                    operation: Operation::Create,
                    possible_duplicate: self.possible_duplicates(
                        &pending_id,
                        record.frontmatter.kind,
                        &record.body,
                        &[],
                    )?,
                };
                let mut tx = self.tx_with_intent(
                    Operation::Create,
                    caller.label(),
                    op,
                    digest,
                    admission.clone(),
                );
                tx.new_record = Some((record.clone(), bytes.clone()));
                tx.receipt = Some(Receipt::from_record(
                    &record,
                    &frontmatter::sha256_hex(&bytes),
                ));
                tx.pending_deletes.push(pending_id.clone());
                let covered_op = tx.intent.op_id.clone();
                self.add_covered_pending(&record, &covered_op, &mut tx)?;
                self.execute_tx(tx)?;
                Ok(admission)
            }
            Operation::Supersede => {
                let targets = parse_target_ids(&proposal.target_ids)?;
                let mut loaded = Vec::new();
                for target_id in &targets {
                    let target = self.load_current(target_id)?.ok_or_else(|| {
                        StoreError::Invalid("pending target is not active".into())
                    })?;
                    if target.trust.trust != Trust::Trusted {
                        return Err(StoreError::ExternalConflict {
                            id: target_id.clone(),
                            reason: "pending target is not trusted".into(),
                        });
                    }
                    if let Some(expected_revision) = proposal.expected_revision
                        && let Some(expected_digest) = &proposal.expected_digest
                        && (target.record.frontmatter.revision != expected_revision
                            || &target.digest != expected_digest)
                    {
                        return Err(StoreError::Conflict {
                            target: target_id.clone(),
                            current: target.cas(),
                            expected: Cas {
                                revision: expected_revision,
                                digest: expected_digest.clone(),
                            },
                        });
                    }
                    loaded.push(target);
                }
                let mut record = pending.clone();
                record.frontmatter.id = pending_id.clone();
                record.frontmatter.status = MemoryStatus::Active;
                record.frontmatter.authority = MemoryAuthority::UserApproved;
                record.frontmatter.proposal = None;
                record.frontmatter.supersedes = targets.iter().map(ToString::to_string).collect();
                record.frontmatter.updated_at = now;
                record.validate()?;
                let bytes = frontmatter::render(&record)?;
                let admission = Admission::Applied {
                    id: pending_id.clone(),
                    revision: 1,
                    operation: Operation::Supersede,
                    possible_duplicate: Vec::new(),
                };
                let mut tx = self.tx_with_intent(
                    Operation::Supersede,
                    caller.label(),
                    op,
                    digest,
                    admission.clone(),
                );
                tx.new_record = Some((record.clone(), bytes.clone()));
                tx.receipt = Some(Receipt::from_record(
                    &record,
                    &frontmatter::sha256_hex(&bytes),
                ));
                for target in loaded {
                    tx.attic.push(attic_entry(
                        &target,
                        MemoryStatus::Superseded,
                        Some(pending_id.clone()),
                        None,
                    ));
                    tx.targets.push(TargetAction {
                        loaded: target,
                        disposition: TargetDisposition::Retire,
                        final_status: Some(MemoryStatus::Superseded),
                        superseded_by: Some(pending_id.clone()),
                        reason: None,
                    });
                }
                tx.pending_deletes.push(pending_id.clone());
                self.execute_tx(tx)?;
                Ok(admission)
            }
            Operation::Update => {
                let target_id = proposal
                    .target_ids
                    .first()
                    .ok_or_else(|| StoreError::Invalid("pending update has no target".into()))?;
                let target_id = MemoryId::parse(target_id.clone())
                    .map_err(|error| StoreError::Invalid(error.to_string()))?;
                let target = self
                    .load_current(&target_id)?
                    .ok_or_else(|| StoreError::Invalid("pending target is not active".into()))?;
                if target.trust.trust != Trust::Trusted {
                    return Err(StoreError::ExternalConflict {
                        id: target_id.clone(),
                        reason: "pending target is not trusted".into(),
                    });
                }
                let expected = Cas {
                    revision: proposal
                        .expected_revision
                        .unwrap_or(target.record.frontmatter.revision),
                    digest: proposal
                        .expected_digest
                        .clone()
                        .unwrap_or_else(|| target.digest.clone()),
                };
                if target.cas() != expected {
                    return Err(StoreError::Conflict {
                        target: target_id,
                        current: target.cas(),
                        expected,
                    });
                }
                // A reviewed identity conversion must not leave two Actives
                // under one subject key. Exact structural equality may be
                // retired into the converted record; anything else needs an
                // explicit supersede, never a forced merge.
                let mut identical_collision = None;
                if let Some(subject_key) = pending.frontmatter.subject_key.as_deref() {
                    let collisions: Vec<Loaded> = self
                        .scan_current(true)?
                        .into_iter()
                        .filter(|loaded| {
                            loaded.trust.trust == Trust::Trusted
                                && loaded.record.frontmatter.id != target_id
                                && loaded.record.frontmatter.kind == pending.frontmatter.kind
                                && loaded.record.frontmatter.subject_key.as_deref()
                                    == Some(subject_key)
                        })
                        .collect();
                    if collisions.len() > 1 {
                        return Err(StoreError::InvariantViolation {
                            subject_key: subject_key.to_string(),
                            ids: collisions
                                .iter()
                                .map(|loaded| loaded.record.frontmatter.id.clone())
                                .collect(),
                        });
                    }
                    if let Some(collision) = collisions.into_iter().next() {
                        let same_value =
                            collision.record.frontmatter.value == pending.frontmatter.value;
                        if !same_value || collision.record.body != pending.body {
                            return Err(StoreError::Invalid(format!(
                                "subject key `{subject_key}` is already used by {}; supersede it explicitly before accepting",
                                collision.record.frontmatter.id
                            )));
                        }
                        identical_collision = Some(collision);
                    }
                }
                let mut record = pending.clone();
                record.frontmatter.id = target_id.clone();
                record.frontmatter.status = MemoryStatus::Active;
                record.frontmatter.revision = target.record.frontmatter.revision + 1;
                record.frontmatter.authority = MemoryAuthority::UserApproved;
                record.frontmatter.proposal = None;
                record.frontmatter.created_at = target.record.frontmatter.created_at;
                record.frontmatter.pinned = target.record.frontmatter.pinned;
                record.frontmatter.pinned_by = target.record.frontmatter.pinned_by;
                record.frontmatter.updated_at = now;
                if let Some(collision) = &identical_collision {
                    record.frontmatter.supersedes =
                        vec![collision.record.frontmatter.id.to_string()];
                }
                record.validate()?;
                let bytes = frontmatter::render(&record)?;
                let admission = Admission::Applied {
                    id: target_id.clone(),
                    revision: record.frontmatter.revision,
                    operation: Operation::Update,
                    possible_duplicate: Vec::new(),
                };
                let mut tx = self.tx_with_intent(
                    Operation::Update,
                    caller.label(),
                    op,
                    digest,
                    admission.clone(),
                );
                tx.new_record = Some((record.clone(), bytes.clone()));
                tx.receipt = Some(Receipt::from_record(
                    &record,
                    &frontmatter::sha256_hex(&bytes),
                ));
                tx.targets.push(TargetAction {
                    loaded: target,
                    disposition: TargetDisposition::Overwritten,
                    final_status: None,
                    superseded_by: None,
                    reason: None,
                });
                if let Some(collision) = identical_collision {
                    tx.attic.push(attic_entry(
                        &collision,
                        MemoryStatus::Superseded,
                        Some(target_id.clone()),
                        None,
                    ));
                    tx.targets.push(TargetAction {
                        loaded: collision,
                        disposition: TargetDisposition::Retire,
                        final_status: Some(MemoryStatus::Superseded),
                        superseded_by: Some(target_id.clone()),
                        reason: None,
                    });
                }
                tx.pending_deletes.push(pending_id);
                self.execute_tx(tx)?;
                Ok(admission)
            }
            Operation::Deprecate | Operation::Forget | Operation::Pin | Operation::Unpin => {
                let target_id = proposal
                    .target_ids
                    .first()
                    .ok_or_else(|| StoreError::Invalid("pending action has no target".into()))?;
                let target_id = MemoryId::parse(target_id.clone())
                    .map_err(|error| StoreError::Invalid(error.to_string()))?;
                let target = self
                    .load_current(&target_id)?
                    .ok_or_else(|| StoreError::Invalid("pending target is not active".into()))?;
                if target.trust.trust != Trust::Trusted {
                    return Err(StoreError::ExternalConflict {
                        id: target_id.clone(),
                        reason: "pending target is not trusted".into(),
                    });
                }
                match proposal.operation {
                    Operation::Deprecate => {
                        let admission = Admission::Applied {
                            id: target_id.clone(),
                            revision: target.record.frontmatter.revision,
                            operation: Operation::Deprecate,
                            possible_duplicate: Vec::new(),
                        };
                        let mut tx = self.tx_with_intent(
                            Operation::Deprecate,
                            caller.label(),
                            op,
                            digest,
                            admission,
                        );
                        tx.targets.push(TargetAction {
                            loaded: target.clone(),
                            disposition: TargetDisposition::Retire,
                            final_status: Some(MemoryStatus::Deprecated),
                            superseded_by: None,
                            reason: Some(proposal.reason.clone()),
                        });
                        tx.attic.push(attic_entry(
                            &target,
                            MemoryStatus::Deprecated,
                            None,
                            Some(proposal.reason.clone()),
                        ));
                        tx.pending_deletes.push(pending_id);
                        self.execute_tx(tx)?;
                        Ok(Admission::Applied {
                            id: target_id,
                            revision: target.record.frontmatter.revision,
                            operation: Operation::Deprecate,
                            possible_duplicate: Vec::new(),
                        })
                    }
                    Operation::Forget => {
                        let tombstone = tombstone_for_loaded(&target);
                        let admission = Admission::Applied {
                            id: target_id.clone(),
                            revision: target.record.frontmatter.revision,
                            operation: Operation::Forget,
                            possible_duplicate: Vec::new(),
                        };
                        let mut tx = self.tx_with_intent(
                            Operation::Forget,
                            caller.label(),
                            op,
                            digest,
                            admission,
                        );
                        tx.targets.push(TargetAction {
                            loaded: target.clone(),
                            disposition: TargetDisposition::Delete,
                            final_status: None,
                            superseded_by: None,
                            reason: None,
                        });
                        tx.tombstones.push(tombstone);
                        tx.delete_dirs
                            .push(self.paths.history_dir_for(&target.record.frontmatter.id));
                        tx.pending_deletes.push(pending_id);
                        self.execute_tx(tx)?;
                        Ok(Admission::Applied {
                            id: target_id,
                            revision: target.record.frontmatter.revision,
                            operation: Operation::Forget,
                            possible_duplicate: Vec::new(),
                        })
                    }
                    Operation::Pin | Operation::Unpin => {
                        let pinned = proposal.operation == Operation::Pin;
                        let owner = if caller.is_user() {
                            PinOwner::User
                        } else {
                            PinOwner::Model
                        };
                        self.pin_commit(
                            op,
                            (*caller).clone(),
                            &target_id,
                            target,
                            owner,
                            pinned,
                            digest,
                        )
                    }
                    _ => unreachable!(),
                }
            }
            other => Err(StoreError::Invalid(format!(
                "unsupported pending operation: {other:?}"
            ))),
        }
    }

    fn reject_pending_locked(
        &self,
        op: OpId,
        caller: Caller,
        pending: MemoryRecord,
        reason: Reason,
        digest: String,
    ) -> Result<Admission, StoreError> {
        let pending_id = pending.frontmatter.id.clone();
        let fingerprint = suppression_fingerprint_for_record(&pending)?;
        // An automatic proposal may have been recorded as a relation against a
        // current record even though the caller proposed it as plain content.
        // The same content must stay suppressed when it is proposed again
        // without that internal relation wrapper.
        let content_fingerprint = suppression_fingerprint(
            pending.frontmatter.kind,
            pending.frontmatter.scope,
            pending.frontmatter.subject_key.as_deref(),
            pending.frontmatter.value.as_deref(),
            &pending.body,
            Operation::Create,
            &[],
        )?;
        let line = serde_json::json!({
            "pending_id": pending_id.to_string(),
            "reason": "rejected",
            "detail": reason.as_str(),
        })
        .to_string();
        let admission = Admission::Rejected {
            reason: RejectReason::Other("rejected_by_user".into()),
        };
        let mut tx = self.tx_with_intent(
            Operation::Review,
            caller.label(),
            op,
            digest,
            admission.clone(),
        );
        tx.pending_deletes.push(pending_id.clone());
        tx.appends.push(super::journal::AppendPlan {
            path: self.paths.suppressed_path.clone(),
            marker: fingerprint.clone(),
            line: fingerprint.clone(),
        });
        if content_fingerprint != fingerprint {
            tx.appends.push(super::journal::AppendPlan {
                path: self.paths.suppressed_path.clone(),
                marker: content_fingerprint.clone(),
                line: content_fingerprint,
            });
        }
        tx.appends.push(super::journal::AppendPlan {
            path: self.paths.pending_history_path.clone(),
            line,
            marker: pending_id.to_string(),
        });
        self.execute_tx(tx)?;
        Ok(admission)
    }

    // ----- transaction engine ------------------------------------------------

    fn tx_with_intent(
        &self,
        operation: Operation,
        caller: impl Into<String>,
        op_id: OpId,
        digest: String,
        admission: Admission,
    ) -> Tx {
        Tx {
            operation,
            caller: caller.into(),
            new_record: None,
            pending_record: None,
            targets: Vec::new(),
            receipt: None,
            tombstones: Vec::new(),
            attic: Vec::new(),
            pending_deletes: Vec::new(),
            delete_dirs: Vec::new(),
            appends: Vec::new(),
            intent: IntentRecord {
                op_id,
                request_digest: digest,
                admission: Some(admission),
                created_at: Utc::now().to_rfc3339(),
            },
        }
    }

    fn commit_pending(
        &self,
        op_id: OpId,
        caller: Caller,
        digest: String,
        record: MemoryRecord,
        bytes: Vec<u8>,
        admission: Admission,
    ) -> Result<Admission, StoreError> {
        let mut tx = self.tx_with_intent(
            Operation::Review,
            caller.label(),
            op_id,
            digest,
            admission.clone(),
        );
        tx.pending_record = Some((record, bytes));
        self.execute_tx(tx)?;
        Ok(admission)
    }

    fn finish_noop(&self, op: &OpId, digest: &str, admission: Admission) -> Result<(), StoreError> {
        let record = IntentRecord {
            op_id: op.clone(),
            request_digest: digest.to_string(),
            admission: Some(admission),
            created_at: Utc::now().to_rfc3339(),
        };
        let bytes =
            serde_json::to_vec(&record).map_err(|error| StoreError::Invalid(error.to_string()))?;
        path::atomic_write(&self.paths.intent_file(op), &bytes)?;
        Ok(())
    }

    fn finish_skip(
        &self,
        op: OpId,
        digest: &str,
        admission: Admission,
        existing: &MemoryId,
    ) -> Result<Admission, StoreError> {
        let mut tx = self.tx_with_intent(
            Operation::Review,
            "runtime",
            op,
            digest.to_string(),
            admission.clone(),
        );
        if let Some(existing) = self.load_current(existing)? {
            let covered_op = tx.intent.op_id.clone();
            self.add_covered_pending(&existing.record, &covered_op, &mut tx)?;
        }
        self.execute_tx(tx)?;
        Ok(admission)
    }

    fn add_covered_pending(
        &self,
        record: &MemoryRecord,
        op: &OpId,
        tx: &mut Tx,
    ) -> Result<(), StoreError> {
        if record.frontmatter.kind != MemoryKind::UserFact {
            return Ok(());
        }
        let Some(subject_key) = record.frontmatter.subject_key.as_deref() else {
            return Ok(());
        };
        for path in path::list_markdown(&self.paths.pending_dir)? {
            let bytes = path::read_bytes(&path)?;
            let pending =
                frontmatter::parse(&bytes).map_err(|error| StoreError::RecoveryConflict {
                    op_id: None,
                    reason: format!("pending document is invalid: {error}"),
                })?;
            if pending.frontmatter.kind != record.frontmatter.kind
                || pending.frontmatter.scope != record.frontmatter.scope
                || pending.frontmatter.subject_key.as_deref() != Some(subject_key)
            {
                continue;
            }
            if pending.frontmatter.value != record.frontmatter.value || pending.body != record.body
            {
                continue;
            }
            let pending_id = pending.frontmatter.id.clone();
            tx.pending_deletes.push(pending_id.clone());
            tx.appends.push(super::journal::AppendPlan {
                path: self.paths.pending_history_path.clone(),
                line: serde_json::json!({
                    "pending_id": pending_id.to_string(),
                    "reason": "covered",
                    "settled_by": op.to_string(),
                    "memory_id": record.frontmatter.id.to_string(),
                })
                .to_string(),
                marker: pending_id.to_string(),
            });
        }
        Ok(())
    }

    fn possible_duplicates(
        &self,
        new_id: &MemoryId,
        kind: MemoryKind,
        body: &str,
        targets: &[TargetAction],
    ) -> Result<Vec<MemoryId>, StoreError> {
        if body.is_empty() {
            return Ok(Vec::new());
        }
        let target_ids: HashSet<&MemoryId> = targets
            .iter()
            .map(|target| &target.loaded.record.frontmatter.id)
            .collect();
        let mut duplicates = Vec::new();
        for loaded in self.scan_current(true)? {
            if loaded.record.frontmatter.kind != kind
                || loaded.record.body != body
                || loaded.record.frontmatter.id == *new_id
                || target_ids.contains(&loaded.record.frontmatter.id)
            {
                continue;
            }
            duplicates.push(loaded.record.frontmatter.id);
        }
        Ok(duplicates)
    }

    fn execute_tx(&self, tx: Tx) -> Result<(), StoreError> {
        let prepared = self.prepare_files(&tx)?;
        let mut journal =
            JournalPlan::new(tx.intent.op_id.clone(), tx.operation, tx.caller.clone());
        journal.new_files = prepared
            .stage
            .as_ref()
            .zip(tx.new_record.as_ref())
            .map(|(stage, (record, bytes))| NewFilePlan {
                id: record.frontmatter.id.clone(),
                revision: record.frontmatter.revision,
                digest: frontmatter::sha256_hex(bytes),
                staged_path: stage.clone(),
                final_path: self.paths.current_file(&record.frontmatter.id),
            })
            .into_iter()
            .collect();
        journal.targets = tx
            .targets
            .iter()
            .map(|target| TargetPlan {
                id: target.loaded.record.frontmatter.id.clone(),
                revision: target.loaded.record.frontmatter.revision,
                digest: target.loaded.digest.clone(),
                disposition: target.disposition,
                final_status: target.final_status,
                superseded_by: target.superseded_by.clone(),
            })
            .collect();
        journal.state_files = prepared.state_files.clone();
        journal.pending_deletes = tx.pending_deletes.clone();
        journal.delete_dirs = tx.delete_dirs.clone();
        journal.appends = tx.appends.clone();
        journal.receipt = tx.receipt.clone();
        journal.intent_record = Some(tx.intent.clone());
        journal.attic = tx.attic.clone();
        journal.tombstones = tx.tombstones.clone();
        let journal_path = self.paths.journal_file(&tx.intent.op_id);
        if let Err(error) = journal.write(&journal_path) {
            self.cleanup_prepared(&prepared);
            return Err(error.into());
        }
        super::failpoints::hit("journal_prepared");
        match self.finish_plan(&mut journal, &journal_path) {
            Ok(()) => Ok(()),
            Err(StoreError::RecoveryConflict { reason, .. })
            | Err(StoreError::ExternalConflict { reason, .. }) => {
                let _ = journal.mark_blocked(&journal_path, reason.clone());
                Err(StoreError::RecoveryConflict {
                    op_id: Some(tx.intent.op_id),
                    reason,
                })
            }
            Err(error) => Err(error),
        }
    }

    fn prepare_files(&self, tx: &Tx) -> Result<PreparedFiles, StoreError> {
        let op = &tx.intent.op_id;
        let mut stage = None;
        let mut state_files = Vec::new();
        let cleanup = |stage: &Option<PathBuf>, state_files: &[StateFilePlan]| {
            if let Some(stage) = stage {
                let _ = path::remove_if_exists(stage);
            }
            for file in state_files {
                let _ = path::remove_if_exists(&file.temp_path);
            }
        };
        let result = (|| -> Result<(), StoreError> {
            if let Some((record, bytes)) = &tx.new_record {
                let stage_path = self.paths.stage_dir.join(format!(
                    "{}-{}.md.stage",
                    &frontmatter::sha256_hex_text(op.as_str())[..16],
                    record.frontmatter.id
                ));
                path::remove_if_exists(&stage_path)?;
                path::write_new(&stage_path, bytes, 0o600)?;
                path::fsync_dir(&self.paths.stage_dir)?;
                stage = Some(stage_path);
            }
            if let Some((record, bytes)) = &tx.new_record {
                let final_path = self
                    .paths
                    .history_file(&record.frontmatter.id, record.frontmatter.revision);
                state_files.push(self.new_state_file(
                    StateFileKind::History {
                        id: record.frontmatter.id.clone(),
                        revision: record.frontmatter.revision,
                    },
                    final_path,
                    bytes,
                    op,
                )?);
            }
            for target in &tx.targets {
                if target.disposition != TargetDisposition::Overwritten {
                    continue;
                }
                let record = &target.loaded.record;
                state_files.push(
                    self.new_state_file(
                        StateFileKind::History {
                            id: record.frontmatter.id.clone(),
                            revision: record.frontmatter.revision,
                        },
                        self.paths
                            .history_file(&record.frontmatter.id, record.frontmatter.revision),
                        &target.loaded.bytes,
                        op,
                    )?,
                );
            }
            if let Some(receipt) = &tx.receipt {
                let bytes = receipt.to_json()?;
                state_files.push(self.new_state_file(
                    StateFileKind::Receipt,
                    self.paths.receipt_file(&receipt.id),
                    &bytes,
                    op,
                )?);
            }
            if let Some((record, bytes)) = &tx.pending_record {
                state_files.push(self.new_state_file(
                    StateFileKind::Pending {
                        id: record.frontmatter.id.clone(),
                    },
                    self.paths.pending_file(&record.frontmatter.id),
                    bytes,
                    op,
                )?);
            }
            for attic in &tx.attic {
                let bytes = serde_json::to_vec(attic)
                    .map_err(|error| StoreError::Invalid(error.to_string()))?;
                state_files.push(self.new_state_file(
                    StateFileKind::Attic {
                        id: attic.id.clone(),
                    },
                    self.paths.attic_file(&attic.id),
                    &bytes,
                    op,
                )?);
            }
            for tombstone in &tx.tombstones {
                let bytes = serde_json::to_vec(tombstone)
                    .map_err(|error| StoreError::Invalid(error.to_string()))?;
                state_files.push(self.new_state_file(
                    StateFileKind::Tombstone {
                        id: tombstone.id.clone(),
                    },
                    self.paths.tombstone_file(&tombstone.id),
                    &bytes,
                    op,
                )?);
            }
            let intent_bytes = serde_json::to_vec(&tx.intent)
                .map_err(|error| StoreError::Invalid(error.to_string()))?;
            state_files.push(self.new_state_file(
                StateFileKind::Intent,
                self.paths.intent_file(op),
                &intent_bytes,
                op,
            )?);
            Ok(())
        })();
        match result {
            Ok(()) => Ok(PreparedFiles { stage, state_files }),
            Err(error) => {
                cleanup(&stage, &state_files);
                Err(error)
            }
        }
    }

    fn new_state_file(
        &self,
        kind: StateFileKind,
        final_path: PathBuf,
        bytes: &[u8],
        op: &OpId,
    ) -> Result<StateFilePlan, StoreError> {
        if let Some(parent) = final_path.parent() {
            fs::create_dir_all(parent).map_err(|error| PathError::Io {
                path: parent.into(),
                source: error,
            })?;
        }
        let name = final_path
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("state");
        let temp_path = final_path
            .parent()
            .expect("state file parent")
            .join(format!(
                ".tmp-{}-{}-{name}",
                &frontmatter::sha256_hex_text(op.as_str())[..16],
                state_tag(&kind)
            ));
        path::remove_if_exists(&temp_path)?;
        path::write_new(&temp_path, bytes, 0o600)?;
        Ok(StateFilePlan {
            kind,
            temp_path,
            final_path,
            digest: frontmatter::sha256_hex(bytes),
        })
    }

    fn cleanup_prepared(&self, prepared: &PreparedFiles) {
        if let Some(stage) = &prepared.stage {
            let _ = path::remove_if_exists(stage);
        }
        for file in &prepared.state_files {
            let _ = path::remove_if_exists(&file.temp_path);
        }
    }

    fn finish_plan(&self, plan: &mut JournalPlan, journal_path: &Path) -> Result<(), StoreError> {
        self.install_new_files(plan)?;
        super::failpoints::hit("current_installed");
        self.retire_targets(plan)?;
        super::failpoints::hit("targets_retired");
        self.finalize_state_files(plan)?;
        self.apply_appends(plan)?;
        self.apply_pending_deletes(plan)?;
        self.apply_delete_dirs(plan)?;
        plan.mark_committed(journal_path)?;
        super::failpoints::hit("journal_committed");
        self.cleanup_prepared_files(plan);
        path::remove_if_exists(journal_path)?;
        path::fsync_dir(&self.paths.journal_dir)?;
        Ok(())
    }

    fn install_new_files(&self, plan: &JournalPlan) -> Result<(), StoreError> {
        for file in &plan.new_files {
            path::reject_symlink(&file.final_path)?;
            path::reject_symlink(&file.staged_path)?;
            let final_exists = self.exists(&file.final_path)?;
            if final_exists {
                let digest = self.digest(&file.final_path)?;
                if digest == file.digest {
                    continue;
                }
                let overwrite = plan.targets.iter().any(|target| {
                    target.id == file.id
                        && target.disposition == TargetDisposition::Overwritten
                        && target.digest == digest
                });
                if !overwrite {
                    return Err(StoreError::RecoveryConflict {
                        op_id: Some(plan.op_id.clone()),
                        reason: format!(
                            "current file for {} changed before the new revision was installed",
                            file.id
                        ),
                    });
                }
            }
            if !self.exists(&file.staged_path)? {
                return Err(StoreError::RecoveryConflict {
                    op_id: Some(plan.op_id.clone()),
                    reason: format!("staged file for {} is missing", file.id),
                });
            }
            if self.digest(&file.staged_path)? != file.digest {
                return Err(StoreError::RecoveryConflict {
                    op_id: Some(plan.op_id.clone()),
                    reason: format!("staged file for {} does not match its digest", file.id),
                });
            }
            fs::rename(&file.staged_path, &file.final_path).map_err(|error| PathError::Io {
                path: file.final_path.clone(),
                source: error,
            })?;
            path::fsync_dir(&self.paths.current_root)?;
        }
        Ok(())
    }

    fn retire_targets(&self, plan: &JournalPlan) -> Result<(), StoreError> {
        for target in &plan.targets {
            match target.disposition {
                TargetDisposition::Overwritten => {
                    let new_file = plan
                        .new_files
                        .iter()
                        .find(|file| file.id == target.id)
                        .ok_or_else(|| StoreError::RecoveryConflict {
                            op_id: Some(plan.op_id.clone()),
                            reason: "overwritten target has no new file".into(),
                        })?;
                    let final_path = self.paths.current_file(&target.id);
                    if !self.exists(&final_path)? {
                        return Err(StoreError::RecoveryConflict {
                            op_id: Some(plan.op_id.clone()),
                            reason: format!("new revision for {} is missing", target.id),
                        });
                    }
                    if self.digest(&final_path)? != new_file.digest {
                        return Err(StoreError::RecoveryConflict {
                            op_id: Some(plan.op_id.clone()),
                            reason: format!("new revision for {} was not installed", target.id),
                        });
                    }
                }
                TargetDisposition::Retire | TargetDisposition::Delete => {
                    let current_path = self.paths.current_file(&target.id);
                    let exists = self.exists(&current_path)?;
                    if exists {
                        let digest = self.digest(&current_path)?;
                        if digest != target.digest {
                            return Err(StoreError::RecoveryConflict {
                                op_id: Some(plan.op_id.clone()),
                                reason: format!(
                                    "target {} changed to a third state; refusing to remove it",
                                    target.id
                                ),
                            });
                        }
                        if target.disposition == TargetDisposition::Retire {
                            self.ensure_history(&target.id, target.revision, &current_path)?;
                        }
                        path::remove_if_exists(&current_path)?;
                        path::fsync_dir(&self.paths.current_root)?;
                    }
                    if target.disposition == TargetDisposition::Delete {
                        super::failpoints::hit("forget_current_removed");
                        self.delete_forgotten_remnants(&target.id)?;
                    }
                }
            }
        }
        Ok(())
    }

    fn ensure_history(
        &self,
        id: &MemoryId,
        revision: u64,
        current_path: &Path,
    ) -> Result<(), StoreError> {
        let history_path = self.paths.history_file(id, revision);
        if self.exists(&history_path)? {
            return Ok(());
        }
        let bytes = path::read_bytes(current_path)?;
        path::atomic_write(&history_path, &bytes)?;
        Ok(())
    }

    fn delete_forgotten_remnants(&self, id: &MemoryId) -> Result<(), StoreError> {
        let _ = path::remove_dir_if_exists(&self.paths.history_dir_for(id))?;
        let _ = path::remove_if_exists(&self.paths.attic_file(id))?;
        let _ = path::remove_if_exists(&self.paths.receipt_file(id))?;
        let _ = path::remove_if_exists(&self.paths.pending_file(id))?;
        Ok(())
    }

    fn finalize_state_files(&self, plan: &JournalPlan) -> Result<(), StoreError> {
        for file in &plan.state_files {
            self.finalize_state_file(plan, file)?;
            match &file.kind {
                StateFileKind::History { .. } => super::failpoints::hit("history_finalized"),
                StateFileKind::Receipt => super::failpoints::hit("receipt_finalized"),
                _ => {}
            }
        }
        Ok(())
    }

    fn finalize_state_file(
        &self,
        plan: &JournalPlan,
        file: &StateFilePlan,
    ) -> Result<(), StoreError> {
        let final_exists = self.exists(&file.final_path)?;
        if final_exists {
            if self.digest(&file.final_path)? == file.digest {
                let _ = path::remove_if_exists(&file.temp_path);
                return Ok(());
            }
            if !state_file_replaceable(&file.kind) {
                return Err(StoreError::RecoveryConflict {
                    op_id: Some(plan.op_id.clone()),
                    reason: format!(
                        "state file {} already exists with a different digest",
                        file.final_path.display()
                    ),
                });
            }
        }
        if self.exists(&file.temp_path)? {
            if self.digest(&file.temp_path)? != file.digest {
                return Err(StoreError::RecoveryConflict {
                    op_id: Some(plan.op_id.clone()),
                    reason: format!(
                        "state temp {} does not match its digest",
                        file.temp_path.display()
                    ),
                });
            }
            if let Some(parent) = file.final_path.parent() {
                fs::create_dir_all(parent).map_err(|error| PathError::Io {
                    path: parent.into(),
                    source: error,
                })?;
            }
            fs::rename(&file.temp_path, &file.final_path).map_err(|error| PathError::Io {
                path: file.final_path.clone(),
                source: error,
            })?;
            if let Some(parent) = file.final_path.parent() {
                path::fsync_dir(parent)?;
            }
            return Ok(());
        }
        let bytes = self.regenerate_state(plan, file)?;
        if frontmatter::sha256_hex(&bytes) != file.digest {
            return Err(StoreError::RecoveryConflict {
                op_id: Some(plan.op_id.clone()),
                reason: "regenerated state content does not match the journal digest".into(),
            });
        }
        path::atomic_write(&file.final_path, &bytes)?;
        Ok(())
    }

    fn regenerate_state(
        &self,
        plan: &JournalPlan,
        file: &StateFilePlan,
    ) -> Result<Vec<u8>, StoreError> {
        let bytes = match &file.kind {
            StateFileKind::History { id, revision } => {
                let current = self.paths.current_file(id);
                if !self.exists(&current)? {
                    return Err(StoreError::RecoveryConflict {
                        op_id: Some(plan.op_id.clone()),
                        reason: format!("history for {id} cannot be regenerated"),
                    });
                }
                let bytes = path::read_bytes(&current)?;
                let record =
                    frontmatter::parse(&bytes).map_err(|error| StoreError::RecoveryConflict {
                        op_id: Some(plan.op_id.clone()),
                        reason: format!("current document is invalid: {error}"),
                    })?;
                if record.frontmatter.id != *id || record.frontmatter.revision != *revision {
                    return Err(StoreError::RecoveryConflict {
                        op_id: Some(plan.op_id.clone()),
                        reason: "current document does not match the planned history".into(),
                    });
                }
                bytes
            }
            StateFileKind::Receipt => plan
                .receipt
                .as_ref()
                .ok_or_else(|| StoreError::RecoveryConflict {
                    op_id: Some(plan.op_id.clone()),
                    reason: "receipt content is missing from the journal".into(),
                })?
                .to_json()?,
            StateFileKind::Intent => {
                serde_json::to_vec(plan.intent_record.as_ref().ok_or_else(|| {
                    StoreError::RecoveryConflict {
                        op_id: Some(plan.op_id.clone()),
                        reason: "intent content is missing from the journal".into(),
                    }
                })?)
                .map_err(|error| StoreError::Invalid(error.to_string()))?
            }
            StateFileKind::Pending { .. } => {
                return Err(StoreError::RecoveryConflict {
                    op_id: Some(plan.op_id.clone()),
                    reason: "pending content cannot be regenerated".into(),
                });
            }
            StateFileKind::Attic { id } => {
                serde_json::to_vec(plan.attic.iter().find(|entry| entry.id == *id).ok_or_else(
                    || StoreError::RecoveryConflict {
                        op_id: Some(plan.op_id.clone()),
                        reason: "attic content is missing from the journal".into(),
                    },
                )?)
                .map_err(|error| StoreError::Invalid(error.to_string()))?
            }
            StateFileKind::Tombstone { id } => serde_json::to_vec(
                plan.tombstones
                    .iter()
                    .find(|entry| entry.id == *id)
                    .ok_or_else(|| StoreError::RecoveryConflict {
                        op_id: Some(plan.op_id.clone()),
                        reason: "tombstone content is missing from the journal".into(),
                    })?,
            )
            .map_err(|error| StoreError::Invalid(error.to_string()))?,
        };
        Ok(bytes)
    }

    fn apply_appends(&self, plan: &JournalPlan) -> Result<(), StoreError> {
        for append in &plan.appends {
            let existing = match path::read_bytes(&append.path) {
                Ok(bytes) => String::from_utf8_lossy(&bytes).to_string(),
                Err(PathError::Io { source, .. })
                    if source.kind() == std::io::ErrorKind::NotFound =>
                {
                    String::new()
                }
                Err(error) => return Err(error.into()),
            };
            if existing.lines().any(|line| line.contains(&append.marker)) {
                continue;
            }
            path::append_line(&append.path, &append.line)?;
        }
        Ok(())
    }

    fn apply_pending_deletes(&self, plan: &JournalPlan) -> Result<(), StoreError> {
        for id in &plan.pending_deletes {
            path::remove_if_exists(&self.paths.pending_file(id))?;
        }
        path::fsync_dir(&self.paths.pending_dir)?;
        Ok(())
    }

    fn apply_delete_dirs(&self, plan: &JournalPlan) -> Result<(), StoreError> {
        for dir in &plan.delete_dirs {
            if !path::is_inside(dir, &self.paths.state_root) {
                return Err(StoreError::RecoveryConflict {
                    op_id: Some(plan.op_id.clone()),
                    reason: "delete directory is outside state root".into(),
                });
            }
            path::remove_dir_if_exists(dir)?;
        }
        Ok(())
    }

    fn cleanup_prepared_files(&self, plan: &JournalPlan) {
        for file in &plan.new_files {
            let _ = path::remove_if_exists(&file.staged_path);
        }
        for file in &plan.state_files {
            let _ = path::remove_if_exists(&file.temp_path);
        }
    }

    fn cleanup_orphans(&self) -> Result<Vec<PathBuf>, StoreError> {
        StoreLifecycleMetrics::count(&STORE_METRICS.orphan_cleanups);
        let mut referenced = HashSet::new();
        for journal_path in path::list_json(&self.paths.journal_dir)? {
            let plan =
                JournalPlan::load(&journal_path).map_err(|error| StoreError::RecoveryConflict {
                    op_id: None,
                    reason: format!("cannot inspect journal for cleanup: {error}"),
                })?;
            for file in &plan.new_files {
                referenced.insert(file.staged_path.clone());
            }
            for file in &plan.state_files {
                referenced.insert(file.temp_path.clone());
            }
        }
        let mut cleaned = Vec::new();
        if let Ok(entries) = fs::read_dir(&self.paths.stage_dir) {
            for entry in entries.flatten() {
                let path = entry.path();
                if referenced.contains(&path) {
                    continue;
                }
                if entry
                    .file_type()
                    .map(|kind| kind.is_file())
                    .unwrap_or(false)
                    && path::remove_if_exists(&path).unwrap_or(false)
                {
                    cleaned.push(path);
                }
            }
        }
        let mut dirs = vec![
            self.paths.state_root.clone(),
            self.paths.history_dir.clone(),
            self.paths.pending_dir.clone(),
            self.paths.attic_dir.clone(),
            self.paths.tombstones_dir.clone(),
            self.paths.receipts_dir.clone(),
            self.paths.intents_dir.clone(),
            self.paths.journal_dir.clone(),
            self.paths.catalog_dir.clone(),
            self.paths.index_dir.clone(),
        ];
        if let Ok(entries) = fs::read_dir(&self.paths.history_dir) {
            for entry in entries.flatten() {
                if entry.file_type().map(|kind| kind.is_dir()).unwrap_or(false) {
                    dirs.push(entry.path());
                }
            }
        }
        for dir in dirs {
            let entries = match fs::read_dir(&dir) {
                Ok(entries) => entries,
                Err(_) => continue,
            };
            for entry in entries.flatten() {
                let name = entry.file_name();
                let name = name.to_string_lossy();
                if !name.starts_with(".tmp-") {
                    continue;
                }
                let path = entry.path();
                if referenced.contains(&path) {
                    continue;
                }
                if entry
                    .file_type()
                    .map(|kind| kind.is_file())
                    .unwrap_or(false)
                    && path::remove_if_exists(&path).unwrap_or(false)
                {
                    cleaned.push(path);
                }
            }
        }
        Ok(cleaned)
    }

    // ----- loading and helpers ----------------------------------------------

    fn scan_current(&self, allow_conflict: bool) -> Result<Vec<Loaded>, StoreError> {
        let mut loaded = Vec::new();
        for path in path::list_markdown(&self.paths.current_root)? {
            let bytes = path::read_bytes(&path)?;
            let record =
                frontmatter::parse(&bytes).map_err(|error| StoreError::ExternalConflict {
                    id: id_from_path(&path),
                    reason: format!("current document is invalid: {error}"),
                })?;
            if record.frontmatter.status != MemoryStatus::Active {
                return Err(StoreError::ExternalConflict {
                    id: record.frontmatter.id,
                    reason: "current root contains a non-active record".into(),
                });
            }
            if record.frontmatter.scope != self.scope {
                return Err(StoreError::ExternalConflict {
                    id: record.frontmatter.id,
                    reason: "record scope does not match its store".into(),
                });
            }
            let receipt = self.load_receipt(&record.frontmatter.id)?;
            let digest = frontmatter::sha256_hex(&bytes);
            let trust = receipt::classify(&record, &digest, receipt.as_ref());
            if trust.trust == Trust::ExternalConflict && !allow_conflict {
                return Err(StoreError::ExternalConflict {
                    id: record.frontmatter.id,
                    reason: "receipt revision, status, or subject_key no longer matches".into(),
                });
            }
            loaded.push(Loaded {
                record,
                bytes,
                digest,
                trust,
            });
        }
        Ok(loaded)
    }

    fn active_by_subject(
        &self,
        kind: MemoryKind,
        subject_key: &str,
        now: DateTime<Utc>,
    ) -> Result<Vec<Loaded>, StoreError> {
        let _ = now;
        Ok(self
            .scan_current(true)?
            .into_iter()
            .filter(|loaded| {
                loaded.trust.trust == Trust::Trusted
                    && loaded.record.frontmatter.kind == kind
                    && loaded.record.frontmatter.subject_key.as_deref() == Some(subject_key)
            })
            .collect())
    }

    /// The trusted direct user record whose stored span this write's user-turn
    /// span contains, if any. Quoting or widening the direct write restates
    /// the same operation; a narrower span is a separate fact.
    fn direct_owner_for(
        &self,
        caller: &Caller,
        intent: &MemoryIntent,
    ) -> Result<Option<Loaded>, StoreError> {
        if caller.is_user() || matches!(caller, Caller::Migration { .. }) {
            return Ok(None);
        }
        let candidate_spans: Vec<&str> = intent
            .evidence
            .iter()
            .filter(|source| source.kind == SourceRefKind::UserTurn)
            .filter_map(|source| source.evidence_span.as_deref())
            .map(str::trim)
            .filter(|span| !span.is_empty())
            .collect();
        if candidate_spans.is_empty() {
            return Ok(None);
        }
        Ok(self.scan_current(true)?.into_iter().find(|loaded| {
            loaded.trust.trust == Trust::Trusted
                && loaded.record.frontmatter.kind == MemoryKind::UserFact
                && loaded.record.frontmatter.subject_key.is_none()
                && matches!(
                    loaded.record.frontmatter.authority,
                    MemoryAuthority::UserExplicit | MemoryAuthority::UserApproved
                )
                && loaded.record.frontmatter.source_refs.iter().any(|source| {
                    source.kind == SourceRefKind::UserTurn
                        && source
                            .evidence_span
                            .as_deref()
                            .map(str::trim)
                            .is_some_and(|stored| {
                                candidate_spans
                                    .iter()
                                    .any(|candidate| candidate.contains(stored))
                            })
                })
        }))
    }

    fn load_current(&self, id: &MemoryId) -> Result<Option<Loaded>, StoreError> {
        let path = self.paths.current_file(id);
        if !self.exists(&path)? {
            if self.tombstone(id)?.is_some() {
                return Err(StoreError::Forgotten(id.clone()));
            }
            if self.exists(&self.paths.attic_file(id))? {
                return Ok(None);
            }
            if self.exists(&self.paths.receipt_file(id))? {
                return Err(StoreError::ExternalConflict {
                    id: id.clone(),
                    reason: "trusted current record was deleted externally".into(),
                });
            }
            return Ok(None);
        }
        let bytes = path::read_bytes(&path)?;
        let record = frontmatter::parse(&bytes).map_err(|error| StoreError::ExternalConflict {
            id: id.clone(),
            reason: format!("current document is invalid: {error}"),
        })?;
        if record.frontmatter.id != *id {
            return Err(StoreError::ExternalConflict {
                id: id.clone(),
                reason: "current document id does not match its path".into(),
            });
        }
        if record.frontmatter.scope != self.scope {
            return Err(StoreError::ExternalConflict {
                id: id.clone(),
                reason: "current document scope does not match its store".into(),
            });
        }
        if record.frontmatter.status != MemoryStatus::Active {
            return Err(StoreError::ExternalConflict {
                id: id.clone(),
                reason: "current root contains a non-active record".into(),
            });
        }
        let receipt = self.load_receipt(id)?;
        let digest = frontmatter::sha256_hex(&bytes);
        let trust = receipt::classify(&record, &digest, receipt.as_ref());
        Ok(Some(Loaded {
            record,
            bytes,
            digest,
            trust,
        }))
    }

    fn load_receipt(&self, id: &MemoryId) -> Result<Option<Receipt>, StoreError> {
        let path = self.paths.receipt_file(id);
        let bytes = match path::read_bytes(&path) {
            Ok(bytes) => bytes,
            Err(PathError::Io { source, .. }) if source.kind() == std::io::ErrorKind::NotFound => {
                return Ok(None);
            }
            Err(error) => return Err(error.into()),
        };
        Receipt::from_json(&bytes)
            .map(Some)
            .map_err(|error| StoreError::ExternalConflict {
                id: id.clone(),
                reason: format!("receipt is invalid: {error}"),
            })
    }

    fn tombstone(&self, id: &MemoryId) -> Result<Option<Tombstone>, StoreError> {
        let path = self.paths.tombstone_file(id);
        let bytes = match path::read_bytes(&path) {
            Ok(bytes) => bytes,
            Err(PathError::Io { source, .. }) if source.kind() == std::io::ErrorKind::NotFound => {
                return Ok(None);
            }
            Err(error) => return Err(error.into()),
        };
        serde_json::from_slice(&bytes)
            .map(Some)
            .map_err(|error| StoreError::ExternalConflict {
                id: id.clone(),
                reason: format!("tombstone is invalid: {error}"),
            })
    }

    fn attic(&self, id: &MemoryId) -> Result<Option<AtticEntry>, StoreError> {
        let path = self.paths.attic_file(id);
        let bytes = match path::read_bytes(&path) {
            Ok(bytes) => bytes,
            Err(PathError::Io { source, .. }) if source.kind() == std::io::ErrorKind::NotFound => {
                return Ok(None);
            }
            Err(error) => return Err(error.into()),
        };
        serde_json::from_slice(&bytes)
            .map(Some)
            .map_err(|error| StoreError::ExternalConflict {
                id: id.clone(),
                reason: format!("attic metadata is invalid: {error}"),
            })
    }

    fn load_pending(&self, id: &MemoryId) -> Result<MemoryRecord, StoreError> {
        let path = self.paths.pending_file(id);
        let bytes = match path::read_bytes(&path) {
            Ok(bytes) => bytes,
            Err(PathError::Io { source, .. }) if source.kind() == std::io::ErrorKind::NotFound => {
                return Err(StoreError::NotFound(id.clone()));
            }
            Err(error) => return Err(error.into()),
        };
        let record = frontmatter::parse(&bytes).map_err(|error| StoreError::RecoveryConflict {
            op_id: None,
            reason: format!("pending document is invalid: {error}"),
        })?;
        if record.frontmatter.id != *id
            || record.frontmatter.status != MemoryStatus::Pending
            || record.frontmatter.proposal.is_none()
        {
            return Err(StoreError::RecoveryConflict {
                op_id: None,
                reason: "pending document identity or status is invalid".into(),
            });
        }
        Ok(record)
    }

    fn pending_targeting(&self, id: &MemoryId) -> Result<Vec<MemoryId>, StoreError> {
        let mut pending = Vec::new();
        for path in path::list_markdown(&self.paths.pending_dir)? {
            let bytes = path::read_bytes(&path)?;
            let record =
                frontmatter::parse(&bytes).map_err(|error| StoreError::RecoveryConflict {
                    op_id: None,
                    reason: format!("pending document is invalid: {error}"),
                })?;
            if record
                .frontmatter
                .proposal
                .as_ref()
                .is_some_and(|proposal| {
                    proposal
                        .target_ids
                        .iter()
                        .any(|target| target == id.as_str())
                })
            {
                pending.push(record.frontmatter.id);
            }
        }
        Ok(pending)
    }

    fn replay_intent(&self, op: &OpId, digest: &str) -> Result<Option<Admission>, StoreError> {
        let path = self.paths.intent_file(op);
        let bytes = match path::read_bytes(&path) {
            Ok(bytes) => bytes,
            Err(PathError::Io { source, .. }) if source.kind() == std::io::ErrorKind::NotFound => {
                return Ok(None);
            }
            Err(error) => return Err(error.into()),
        };
        let record: IntentRecord =
            serde_json::from_slice(&bytes).map_err(|error| StoreError::RecoveryConflict {
                op_id: Some(op.clone()),
                reason: format!("intent record is invalid: {error}"),
            })?;
        if record.op_id != *op {
            return Err(StoreError::RecoveryConflict {
                op_id: Some(op.clone()),
                reason: "intent record does not match its filename".into(),
            });
        }
        if record.request_digest != digest {
            return Err(StoreError::OpIdReuse(op.clone()));
        }
        let admission = record
            .admission
            .ok_or_else(|| StoreError::RecoveryConflict {
                op_id: Some(op.clone()),
                reason: "intent record has no outcome".into(),
            })?;
        Ok(Some(match admission {
            Admission::Applied {
                id,
                revision,
                operation,
                ..
            } => Admission::Replayed {
                id,
                revision,
                operation,
            },
            other => other,
        }))
    }

    fn is_suppressed(&self, fingerprint: &str) -> Result<bool, StoreError> {
        let bytes = match path::read_bytes(&self.paths.suppressed_path) {
            Ok(bytes) => bytes,
            Err(PathError::Io { source, .. }) if source.kind() == std::io::ErrorKind::NotFound => {
                return Ok(false);
            }
            Err(error) => return Err(error.into()),
        };
        let text = String::from_utf8_lossy(&bytes);
        Ok(text.lines().any(|line| line.trim() == fingerprint))
    }

    fn non_current_error(&self, id: &MemoryId) -> Result<ReadRecord, StoreError> {
        if let Some(tombstone) = self.tombstone(id)? {
            let _ = tombstone;
            return Err(StoreError::Forgotten(id.clone()));
        }
        if let Some(attic) = self.attic(id)? {
            return Err(StoreError::NotCurrent {
                id: id.clone(),
                status: attic.status,
            });
        }
        Err(StoreError::NotFound(id.clone()))
    }

    fn summary(&self, loaded: &Loaded, now: DateTime<Utc>) -> MemorySummary {
        let _ = now;
        MemorySummary {
            id: loaded.record.frontmatter.id.clone(),
            kind: loaded.record.frontmatter.kind,
            scope: loaded.record.frontmatter.scope,
            status: loaded.record.frontmatter.status,
            revision: loaded.record.frontmatter.revision,
            title: loaded.record.frontmatter.title.clone(),
            subject: loaded.record.frontmatter.subject.clone(),
            subject_key: loaded.record.frontmatter.subject_key.clone(),
            value: loaded.record.frontmatter.value.clone(),
            tags: loaded.record.frontmatter.tags.clone(),
            authority: loaded.record.frontmatter.authority,
            effective_authority: loaded.trust.effective_authority,
            trust: loaded.trust.trust.into(),
            expires_at: loaded.record.frontmatter.expires_at,
            updated_at: loaded.record.frontmatter.updated_at,
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn build_record(
        &self,
        intent: &MemoryIntent,
        id: MemoryId,
        revision: u64,
        status: MemoryStatus,
        authority: MemoryAuthority,
        created_at: DateTime<Utc>,
        updated_at: DateTime<Utc>,
        proposal: Option<Proposal>,
        supersedes: Vec<String>,
    ) -> Result<MemoryRecord, StoreError> {
        let record = MemoryRecord {
            frontmatter: Frontmatter {
                schema: super::record::SCHEMA_VERSION,
                id,
                kind: intent.kind,
                scope: intent.scope,
                status,
                revision,
                authority,
                title: intent.title.clone(),
                subject: intent.subject.clone(),
                subject_key: intent.subject_key.clone(),
                value: intent.value.clone(),
                statement: intent.statement,
                tags: intent.tags.clone(),
                pinned: false,
                pinned_by: None,
                sensitive: false,
                expires_at: intent.expires_at,
                supersedes,
                superseded_by: None,
                proposal,
                experience: intent.experience.clone(),
                source_refs: intent.evidence.clone(),
                created_at,
                updated_at,
                extra: serde_yaml::Mapping::new(),
            },
            body: intent.body.clone(),
        };
        record.validate()?;
        Ok(record)
    }

    fn build_record_from_record(
        &self,
        source: &MemoryRecord,
        revision: u64,
        status: MemoryStatus,
        authority: MemoryAuthority,
        updated_at: DateTime<Utc>,
        proposal: Option<Proposal>,
    ) -> Result<MemoryRecord, StoreError> {
        let mut frontmatter = source.frontmatter.clone();
        frontmatter.revision = revision;
        frontmatter.status = status;
        frontmatter.authority = authority;
        frontmatter.proposal = proposal;
        frontmatter.updated_at = updated_at;
        let record = MemoryRecord {
            frontmatter,
            body: source.body.clone(),
        };
        record.validate()?;
        Ok(record)
    }

    fn exists(&self, path: &Path) -> Result<bool, StoreError> {
        match fs::symlink_metadata(path) {
            Ok(metadata) => {
                if metadata.file_type().is_symlink() {
                    return Err(PathError::Symlink(path.into()).into());
                }
                Ok(true)
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
            Err(error) => Err(PathError::Io {
                path: path.into(),
                source: error,
            }
            .into()),
        }
    }

    fn digest(&self, path: &Path) -> Result<String, StoreError> {
        let bytes = path::read_bytes(path)?;
        Ok(frontmatter::sha256_hex(&bytes))
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct AdmitRequest {
    pub op_id: OpId,
    pub caller: Caller,
    pub intent: MemoryIntent,
    pub expected: Option<Cas>,
}

#[derive(Debug, Clone, Serialize)]
struct DestructiveActionTag<'a> {
    action: &'a str,
    reason: Option<&'a str>,
    pinned_by: Option<PinOwner>,
}

#[derive(Debug, Clone)]
enum DestructiveAction {
    Deprecate { reason: Reason },
    Forget,
    Pin { pinned_by: PinOwner },
    Unpin,
}

impl DestructiveAction {
    fn operation(&self) -> Operation {
        match self {
            Self::Deprecate { .. } => Operation::Deprecate,
            Self::Forget => Operation::Forget,
            Self::Pin { .. } => Operation::Pin,
            Self::Unpin => Operation::Unpin,
        }
    }

    fn tag(&self) -> DestructiveActionTag<'_> {
        match self {
            Self::Deprecate { reason } => DestructiveActionTag {
                action: "deprecate",
                reason: Some(reason.as_str()),
                pinned_by: None,
            },
            Self::Forget => DestructiveActionTag {
                action: "forget",
                reason: None,
                pinned_by: None,
            },
            Self::Pin { pinned_by } => DestructiveActionTag {
                action: "pin",
                reason: None,
                pinned_by: Some(*pinned_by),
            },
            Self::Unpin => DestructiveActionTag {
                action: "unpin",
                reason: None,
                pinned_by: None,
            },
        }
    }
}

impl Serialize for DestructiveAction {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        self.tag().serialize(serializer)
    }
}

enum JournalCondition {
    Clean,
    Prepared,
    Blocked,
}

fn plan_id(plan: &JournalPlan) -> MemoryId {
    plan.new_files
        .first()
        .map(|file| file.id.clone())
        .or_else(|| plan.targets.first().map(|target| target.id.clone()))
        .unwrap_or_else(|| MemoryId::parse("mem_receipt_scope").expect("static id"))
}

fn relation_operation(relation: &Relation) -> Operation {
    match relation {
        Relation::None => Operation::Create,
        Relation::Update { .. } => Operation::Update,
        Relation::Supersede { .. } | Relation::Replaces { .. } => Operation::Supersede,
    }
}

fn relation_targets(relation: &Relation) -> Vec<String> {
    match relation {
        Relation::None => Vec::new(),
        Relation::Update { target } => vec![target.to_string()],
        Relation::Supersede { targets } => targets.iter().map(ToString::to_string).collect(),
        Relation::Replaces { target } => vec![target.to_string()],
    }
}

/// The authority a NEW value carries for this caller and this intent.
///
/// Derivation stays on the caller path — a claimed `proposed_authority` never
/// mints user authority — and a runtime user fact additionally has to prove
/// that its body is the user's own sentence. When it cannot, the value is the
/// runtime's proposal, and a proposal is what it is recorded as.
fn creation_authority(caller: &Caller, kind: MemoryKind, intent: &MemoryIntent) -> MemoryAuthority {
    let proven = lifecycle::user_statement_is_proven(intent);
    match lifecycle::creation_decision(
        caller,
        kind,
        intent.scope,
        intent.experience.as_ref(),
        proven,
    ) {
        CreationDecision::Active(authority) => authority,
        CreationDecision::Pending(_) | CreationDecision::Rejected(_) => match (caller, kind) {
            (Caller::Runtime { .. }, MemoryKind::UserFact) => MemoryAuthority::ModelInferred,
            _ => caller.authority_for(
                kind,
                intent
                    .experience
                    .as_ref()
                    .is_some_and(Experience::has_resolution),
            ),
        },
    }
}

/// The authority an existing target grants its caller for a destructive action.
fn authority_for_caller(
    caller: &Caller,
    kind: MemoryKind,
    experience: Option<&Experience>,
) -> MemoryAuthority {
    match lifecycle::creation_decision(
        caller,
        kind,
        MemoryScope::Project,
        experience,
        // A destructive action says nothing about a new user statement; this
        // path only needs the caller's own ceiling.
        false,
    ) {
        CreationDecision::Active(authority) => authority,
        CreationDecision::Pending(_) | CreationDecision::Rejected(_) => {
            caller.authority_for(kind, experience.is_some_and(Experience::has_resolution))
        }
    }
}

fn intent_from_patch(record: &MemoryRecord, patch: &RecordPatch) -> MemoryIntent {
    MemoryIntent {
        kind: record.frontmatter.kind,
        scope: record.frontmatter.scope,
        subject: patch
            .subject
            .clone()
            .or_else(|| record.frontmatter.subject.clone()),
        subject_key: patch
            .subject_key
            .clone()
            .or_else(|| record.frontmatter.subject_key.clone()),
        value: patch
            .value
            .clone()
            .or_else(|| record.frontmatter.value.clone()),
        title: patch
            .title
            .clone()
            .unwrap_or_else(|| record.frontmatter.title.clone()),
        body: patch.body.clone().unwrap_or_else(|| record.body.clone()),
        tags: patch
            .tags
            .clone()
            .unwrap_or_else(|| record.frontmatter.tags.clone()),
        statement: record.frontmatter.statement,
        relation: Relation::Update {
            target: record.frontmatter.id.clone(),
        },
        expires_at: match &patch.expires_at {
            Some(value) => *value,
            None => record.frontmatter.expires_at,
        },
        evidence: patch
            .evidence
            .clone()
            .unwrap_or_else(|| record.frontmatter.source_refs.clone()),
        experience: record.frontmatter.experience.clone(),
        proposed_authority: record.frontmatter.authority,
    }
}

fn intent_from_new(new: NewRecord, relation: Relation) -> MemoryIntent {
    MemoryIntent {
        kind: new.kind,
        scope: new.scope,
        subject: new.subject,
        subject_key: new.subject_key,
        value: new.value,
        title: new.title,
        body: new.body,
        tags: new.tags,
        statement: new.statement,
        relation,
        expires_at: new.expires_at,
        evidence: new.evidence,
        experience: new.experience,
        proposed_authority: MemoryAuthority::ModelInferred,
    }
}

fn request_digest<T: Serialize>(value: &T) -> Result<String, StoreError> {
    let bytes =
        serde_json::to_vec(value).map_err(|error| StoreError::Invalid(error.to_string()))?;
    Ok(frontmatter::sha256_hex(&bytes))
}

fn suppression_fingerprint(
    kind: MemoryKind,
    scope: MemoryScope,
    subject_key: Option<&str>,
    value: Option<&str>,
    body: &str,
    operation: Operation,
    targets: &[String],
) -> Result<String, StoreError> {
    let mut targets = targets.to_vec();
    targets.sort();
    #[derive(Serialize)]
    struct Fingerprint<'a> {
        kind: MemoryKind,
        scope: MemoryScope,
        subject_key: Option<&'a str>,
        value: Option<&'a str>,
        body: &'a str,
        operation: Operation,
        targets: Vec<String>,
    }
    request_digest(&Fingerprint {
        kind,
        scope,
        subject_key,
        value,
        body,
        operation,
        targets,
    })
}

fn suppression_fingerprint_for_record(record: &MemoryRecord) -> Result<String, StoreError> {
    let proposal = record
        .frontmatter
        .proposal
        .as_ref()
        .ok_or_else(|| StoreError::Invalid("pending record has no proposal".into()))?;
    suppression_fingerprint(
        record.frontmatter.kind,
        record.frontmatter.scope,
        record.frontmatter.subject_key.as_deref(),
        record.frontmatter.value.as_deref(),
        &record.body,
        proposal.operation,
        &proposal.target_ids,
    )
}

fn attic_entry(
    loaded: &Loaded,
    status: MemoryStatus,
    superseded_by: Option<MemoryId>,
    reason: Option<String>,
) -> AtticEntry {
    AtticEntry {
        id: loaded.record.frontmatter.id.clone(),
        kind: loaded.record.frontmatter.kind,
        scope: loaded.record.frontmatter.scope,
        revision: loaded.record.frontmatter.revision,
        digest: loaded.digest.clone(),
        status,
        superseded_by,
        reason,
        updated_at: Utc::now().to_rfc3339(),
    }
}

fn tombstone_for_loaded(loaded: &Loaded) -> Tombstone {
    Tombstone {
        id: loaded.record.frontmatter.id.clone(),
        kind: loaded.record.frontmatter.kind,
        scope: loaded.record.frontmatter.scope,
        subject_key_sha256: loaded
            .record
            .frontmatter
            .subject_key
            .as_deref()
            .map(frontmatter::sha256_hex_text),
        last_revision: loaded.record.frontmatter.revision,
        content_sha256: loaded.digest.clone(),
        forgotten_at: Utc::now().to_rfc3339(),
    }
}

fn parse_target_ids(targets: &[String]) -> Result<Vec<MemoryId>, StoreError> {
    targets
        .iter()
        .map(|target| {
            MemoryId::parse(target.clone()).map_err(|error| StoreError::Invalid(error.to_string()))
        })
        .collect()
}

fn id_from_path(path: &Path) -> MemoryId {
    let name = path
        .file_stem()
        .and_then(|name| name.to_str())
        .unwrap_or("mem_unknown");
    MemoryId::parse(name)
        .unwrap_or_else(|_| MemoryId::parse("mem_unknown").expect("static fallback id is valid"))
}

fn state_file_replaceable(kind: &StateFileKind) -> bool {
    matches!(kind, StateFileKind::Receipt | StateFileKind::Pending { .. })
}

fn state_tag(kind: &StateFileKind) -> &'static str {
    match kind {
        StateFileKind::History { .. } => "history",
        StateFileKind::Receipt => "receipt",
        StateFileKind::Intent => "intent",
        StateFileKind::Pending { .. } => "pending",
        StateFileKind::Attic { .. } => "attic",
        StateFileKind::Tombstone { .. } => "tombstone",
    }
}
