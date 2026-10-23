//! Journal plans for cross-file recovery.
//!
//! A journal never decides semantics. It records the already-decided paths so
//! recovery can finish or refuse a transaction without inventing new content.

use std::path::PathBuf;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use super::path::{self, PathError};
use super::receipt::Receipt;
use super::record::{
    IntentRecord, MemoryId, MemoryKind, MemoryScope, MemoryStatus, OpId, Operation, Tombstone,
};

pub const JOURNAL_SCHEMA: u32 = 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum JournalState {
    Prepared,
    Committed,
    Blocked,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TargetDisposition {
    /// Remove the current file and keep history/attic (supersede/deprecate/expire).
    Retire,
    /// Remove every body copy (forget).
    Delete,
    /// The same id is replaced in place by a new file revision.
    Overwritten,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TargetPlan {
    pub id: MemoryId,
    pub revision: u64,
    pub digest: String,
    pub disposition: TargetDisposition,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub final_status: Option<super::record::MemoryStatus>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub superseded_by: Option<MemoryId>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NewFilePlan {
    pub id: MemoryId,
    pub revision: u64,
    pub digest: String,
    pub staged_path: PathBuf,
    pub final_path: PathBuf,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StateFileKind {
    History { id: MemoryId, revision: u64 },
    Receipt,
    Intent,
    Pending { id: MemoryId },
    Attic { id: MemoryId },
    Tombstone { id: MemoryId },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StateFilePlan {
    pub kind: StateFileKind,
    pub temp_path: PathBuf,
    pub final_path: PathBuf,
    pub digest: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AtticEntry {
    pub id: MemoryId,
    pub kind: MemoryKind,
    pub scope: MemoryScope,
    pub revision: u64,
    pub digest: String,
    pub status: MemoryStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub superseded_by: Option<MemoryId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    pub updated_at: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AppendPlan {
    pub path: PathBuf,
    pub line: String,
    /// Recovery skips the append when the line already exists.
    pub marker: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct JournalPlan {
    pub schema: u32,
    pub op_id: OpId,
    pub operation: Operation,
    pub state: JournalState,
    pub caller: String,
    pub created_at: DateTime<Utc>,
    pub new_files: Vec<NewFilePlan>,
    pub targets: Vec<TargetPlan>,
    pub state_files: Vec<StateFilePlan>,
    pub pending_deletes: Vec<MemoryId>,
    pub delete_dirs: Vec<PathBuf>,
    pub appends: Vec<AppendPlan>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub receipt: Option<Receipt>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub intent_record: Option<IntentRecord>,
    #[serde(default)]
    pub attic: Vec<AtticEntry>,
    #[serde(default)]
    pub tombstones: Vec<Tombstone>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub blocked_reason: Option<String>,
}

impl JournalPlan {
    pub fn new(op_id: OpId, operation: Operation, caller: impl Into<String>) -> Self {
        Self {
            schema: JOURNAL_SCHEMA,
            op_id,
            operation,
            state: JournalState::Prepared,
            caller: caller.into(),
            created_at: Utc::now(),
            new_files: Vec::new(),
            targets: Vec::new(),
            state_files: Vec::new(),
            pending_deletes: Vec::new(),
            delete_dirs: Vec::new(),
            appends: Vec::new(),
            receipt: None,
            intent_record: None,
            attic: Vec::new(),
            tombstones: Vec::new(),
            blocked_reason: None,
        }
    }

    pub fn write(&self, path: &std::path::Path) -> Result<(), PathError> {
        let bytes = serde_json::to_vec(self)
            .map_err(|error| PathError::ScopeUnavailable(format!("journal encode: {error}")))?;
        path::atomic_write(path, &bytes)
    }

    pub fn load(path: &std::path::Path) -> Result<Self, PathError> {
        let bytes = path::read_bytes(path)?;
        serde_json::from_slice(&bytes)
            .map_err(|error| PathError::ScopeUnavailable(format!("journal decode: {error}")))
    }

    pub fn mark_blocked(
        &mut self,
        path: &std::path::Path,
        reason: impl Into<String>,
    ) -> Result<(), PathError> {
        self.state = JournalState::Blocked;
        self.blocked_reason = Some(reason.into());
        self.write(path)
    }

    pub fn mark_committed(&mut self, path: &std::path::Path) -> Result<(), PathError> {
        self.state = JournalState::Committed;
        self.write(path)
    }
}

#[derive(Debug, Clone, Default)]
pub struct RecoveryReport {
    pub recovered: Vec<OpId>,
    pub cleaned: Vec<PathBuf>,
    pub blocked: Vec<OpId>,
}

impl RecoveryReport {
    pub fn is_blocked(&self) -> bool {
        !self.blocked.is_empty()
    }
}

pub fn tombstone_for(
    id: MemoryId,
    kind: super::record::MemoryKind,
    scope: super::record::MemoryScope,
    subject_key_sha256: Option<String>,
    last_revision: u64,
    content_sha256: String,
) -> Tombstone {
    Tombstone {
        id,
        kind,
        scope,
        subject_key_sha256,
        last_revision,
        content_sha256,
        forgotten_at: Utc::now().to_rfc3339(),
    }
}
