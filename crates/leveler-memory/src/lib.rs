//! Cross-session memory for CodeLeveler.
//!
//! The pipeline is isolated from the model's conversation: an observation
//! inbox receives durable facts extracted after each turn, Dream consolidates
//! those observations into Markdown topics, and a generated `MEMORY.md`
//! manifest gives the agent a compact index it can read on demand.
//!
//! Data lives under the CodeLeveler home (`<LevelerHome>/memory/`) and is
//! never written into the user's repository.

#![deny(clippy::indexing_slicing)]

pub mod access;
mod access_policy;
pub mod batch_dream;
mod batch_dream_catalog;
mod batch_dream_commit;
mod batch_dream_control;
mod batch_dream_io;
mod batch_dream_outline;
mod batch_dream_plan;
mod batch_dream_recovery;
mod batch_dream_session;
mod browse;
pub mod capture_eligibility;
pub mod capture_model;
pub mod capture_store;
pub mod capture_transcript;
pub mod chunker;
mod clock;
pub mod config;
pub mod consolidation;
pub mod dream_lock;
pub mod index;
mod maintenance;
pub mod observation;
mod query_expansion;
pub mod schema;
pub mod scope;
pub mod sqlite_journal;
pub mod storage;
pub mod text_utils;
mod topic_reads;

pub use access::{MemoryAccess, MemoryAccessResource, MemoryWrite};
pub use access_policy::{
    MemoryAccessError, MemoryAccessPolicy, MemoryBoundedRead, MemoryPathClass,
};
pub use capture_eligibility::{CaptureAdmission, CapturePlanStep, plan_capture_admissions};
pub use capture_store::{
    CaptureCursors, CaptureError, CaptureJob, CaptureLease, CaptureOutcomeDraft, CaptureRange,
    CaptureStore, CaptureWorkState, ClaimRequest, CommitResult, MAX_ALIASES, MAX_BODY_BYTES,
    MAX_KEYWORDS, MAX_OBSERVATIONS, MAX_STATEMENT_BYTES, MAX_TERM_BYTES, MAX_TOPIC_BYTES,
    ObservationDraft, ObservationType,
};
pub use clock::{MemoryClock, SharedMemoryClock, SystemMemoryClock, system_memory_clock};
pub use consolidation::{
    ClaimedObservation, ConsolidationError, ConsolidationInput, ConsolidationLease,
    ConsolidationResult, ConsolidationStatus, ConsolidationStore, DreamClaimRequest,
    DreamEligibility, DreamEligibilityConfig, DreamTriggerDisposition, TopicOperation,
};
pub use index::{ChunkRecord, FtsResult, MemoryIndex, ReindexResult};
pub use maintenance::{
    DreamLeaseState, ForgetReason, ForgetRequest, ForgetResult, GcResult, MAX_FORGET_FILE_BYTES,
    MaintenanceError, MaintenanceStore, MemoryScopeStatus, RetentionPolicy,
};
pub use observation::*;
pub use scope::{
    MAX_MANUAL_OBSERVATION_BYTES, ManifestBudget, ManifestSource, MemoryManifest, MemoryScope,
    MemoryScopeError, collect_scope_manifest_source, content_hash, ensure_scope_initialized,
    ensure_scope_initialized_with_journal_mode, persist_scope_manifest, regenerate_scope_manifest,
    render_manifest_from_source, render_scope_manifest, set_configured_manifest_budget,
};
pub use storage::MemoryStorage;

pub(crate) const MEMORY_LOG_TARGET: &str = "leveler_memory";
