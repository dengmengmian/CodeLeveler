//! Memory 2.0 Stage 1: Markdown store foundation.
//!
//! This module is isolated from the legacy store and from the agent runtime.
//! Markdown current records are the content authority; private state files hold
//! history, receipts, pending proposals, journals and tombstones.

pub mod catalog;
pub(crate) mod failpoints;
pub mod frontmatter;
pub mod index;
pub mod journal;
pub mod lifecycle;
pub mod path;
pub mod receipt;
pub mod record;
pub mod service;
pub mod store;

pub use catalog::{CatalogChange, CatalogEntry, CatalogState, CatalogUpdate};
pub use frontmatter::{DocumentError, sha256_hex, sha256_hex_text};
pub use index::{DerivedRow, IndexDiagnostics, IndexError, IndexHit, MemoryIndex, ReconcileReport};
pub use journal::{JournalPlan, JournalState, RecoveryReport};
pub use lifecycle::{Admission, PendingReason, RejectReason, SkipReason};
pub use path::{PathError, ScopeRoots, StorePaths};
pub use receipt::{Receipt, Trust, TrustView};
pub use record::{
    Caller, Cas, Experience, ExperienceEvidence, Frontmatter, IntentRecord, MemoryAuthority,
    MemoryId, MemoryIntent, MemoryKind, MemoryRecord, MemoryScope, MemoryStatus, OpId, Operation,
    PinOwner, Proposal, Relation, SourceRef, SourceRefKind, StatementKind, Tombstone,
};
pub use service::{
    Actor, IDENTITY_CONTEXT_LIMIT, IdentityRef, MemoryService, MigrationId, UserInvocationId,
    WriteIntent,
};
pub use store::{
    AdmitRequest, DiagnosticIssue, DiagnosticKind, DiagnosticReport, DiagnosticSeverity,
    EffectiveRecord, ForgetOutcome, ListQuery, MarkdownStore, MemorySummary, NewRecord, ReadRecord,
    Reason, RecordPatch, ReviewDecision, StoreError, StoreLifecycleMetrics, StoreLifecycleSnapshot,
    TrustSummary, lifecycle_metrics,
};
