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
pub mod batch_dream;
mod batch_dream_catalog;
mod batch_dream_commit;
mod batch_dream_control;
mod batch_dream_io;
mod batch_dream_outline;
mod batch_dream_plan;
mod batch_dream_recovery;
mod batch_dream_session;
pub mod capture_model;
pub mod capture_transcript;
pub mod chunker;
pub mod config;
pub mod dream_lock;
pub mod index;
pub mod observation;
mod query_expansion;
pub mod schema;
pub mod sqlite_journal;
pub mod storage;
mod storage_v2;
pub mod text_utils;
pub mod v2;
mod v2_access;
pub mod v2_capture;
mod v2_clock;
pub mod v2_consolidation;
mod v2_maintenance;
mod v2_topic_reads;

pub use access::{MemoryV2Access, MemoryV2AccessResource, MemoryV2Write};
pub use index::{ChunkRecord, FtsResult, MemoryIndex, ReindexResult};
pub use observation::*;
pub use storage::MemoryStorage;
pub use v2::{
    MAX_MANUAL_OBSERVATION_BYTES, V2Manifest, V2ManifestBudget, V2ManifestSource, V2MemoryScope,
    V2StorageError, collect_scope_manifest_source, content_hash, ensure_scope_initialized,
    ensure_scope_initialized_with_journal_mode, persist_scope_manifest, regenerate_scope_manifest,
    render_manifest_from_source, render_scope_manifest, set_configured_manifest_budget,
};
pub use v2_access::{V2AccessError, V2BoundedRead, V2MemoryAccessPolicy, V2PathClass};
pub use v2_capture::{
    CaptureCursors, CaptureJob, CaptureLease, CaptureOutcomeDraft, CaptureRange, CaptureWorkState,
    ClaimRequest, CommitResult, MAX_ALIASES, MAX_BODY_BYTES, MAX_KEYWORDS, MAX_OBSERVATIONS,
    MAX_STATEMENT_BYTES, MAX_TERM_BYTES, MAX_TOPIC_BYTES, ObservationDraft, ObservationType,
    V2CaptureError, V2CaptureStore,
};
pub use v2_clock::{SharedV2Clock, SystemV2Clock, V2Clock, system_v2_clock};
pub use v2_consolidation::{
    ClaimedObservation, ConsolidationInput, ConsolidationLease, ConsolidationResult,
    ConsolidationStatus, DreamClaimRequest, DreamEligibility, DreamEligibilityConfig,
    DreamTriggerDisposition, TopicOperation, V2ConsolidationError, V2ConsolidationStore,
};
pub use v2_maintenance::{
    DreamLeaseState, ForgetReason, ForgetRequest, ForgetResult, GcResult, MAX_FORGET_FILE_BYTES,
    RetentionPolicy, V2MaintenanceError, V2MaintenanceStore, V2ScopeStatus,
};

pub(crate) const MEMORY_LOG_TARGET: &str = "leveler_memory";
