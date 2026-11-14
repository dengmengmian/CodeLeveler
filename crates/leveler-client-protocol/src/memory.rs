//! Memory inspection and management wire types.
//!
//! The TUI (and any future client) never reads the memory store directly.
//! The runtime projects durable state into these content-bearing but
//! mechanically derived values; file contents are fetched per file with a
//! correlation id so a page paints without loading the whole scope.

use serde::{Deserialize, Serialize};

/// One scope's health and counters.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct UiMemoryScope {
    /// `global` or `workspace`.
    pub scope: String,
    pub root: String,
    /// Sorted topic/observation files that may be previewed.
    pub files: Vec<UiMemoryFile>,
    pub pending_capture_jobs: u64,
    pub running_capture_jobs: u64,
    pub retry_capture_jobs: u64,
    pub failed_capture_jobs: u64,
    pub completed_capture_jobs: u64,
    pub pending_observations: u64,
    pub hidden_pending_observations: u64,
    pub archive_count: u64,
    pub tombstone_count: u64,
    pub dream_lease: String,
    pub last_success_at: Option<i64>,
    pub last_failure_at: Option<i64>,
    /// The scope could not be opened; the page shows this instead of zeros.
    pub error: Option<String>,
}

/// One file a memory page may list and preview.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct UiMemoryFile {
    pub scope: String,
    /// Path relative to the scope root, with `/` separators.
    pub path: String,
    /// `manifest`, `topic`, `observation`, `archive`, or `other`.
    pub kind: String,
    pub bytes: u64,
    /// Unix seconds, when the platform reports it.
    pub modified_at: Option<i64>,
    /// The runtime would accept a forget for these bytes. Manifests, protected
    /// files and files above the store's forget bound are not deletable.
    pub deletable: bool,
}

/// One session's durable capture cursors and queued work.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct UiMemorySession {
    pub session_id: String,
    pub requested: u32,
    pub captured: u32,
    pub indexed: u32,
    pub pending: u32,
    pub active: u32,
    pub failed: u32,
}

/// What the memory pipeline is doing for this session.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct UiMemoryActivity {
    /// `idle`, `capturing`, `dreaming`, or `failed`.
    pub kind: String,
    pub active: bool,
    pub last_error: Option<String>,
    pub last_captured_range: Option<(u32, u32)>,
    pub last_dream_at: Option<i64>,
}

/// Everything the memory page lists without opening a file.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct UiMemoryStatus {
    /// The configured rollout: `off`, `record_only`, `shadow`, or `active`.
    pub rollout: String,
    /// Memory is on for this session. False when the configured rollout is
    /// `off`, or when the session toggle paused it; `rollout` distinguishes
    /// the two.
    pub enabled: bool,
    /// The global config file this runtime loaded and watches. The page uses
    /// it when it explains how to change the rollout, so the instruction names
    /// the file that will actually take effect.
    pub config_path: String,
    pub capture_enabled: bool,
    pub automatic_dream_enabled: bool,
    pub manual_dream_enabled: bool,
    pub batch_dream_enabled: bool,
    pub workspace_ephemeral: bool,
    pub global: UiMemoryScope,
    pub workspace: UiMemoryScope,
    pub session: Option<UiMemorySession>,
    pub activity: UiMemoryActivity,
}

/// One file's contents, bounded by the runtime.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct UiMemoryFileContent {
    pub scope: String,
    pub path: String,
    pub content: String,
    /// BLAKE3 of the bytes that produced `content`. Forget sends this back so
    /// the tombstone names exactly the content the user saw.
    pub content_hash: String,
    /// The file was larger than the projection cap; `content` is its prefix.
    pub truncated: bool,
    pub total_bytes: u64,
    /// The read failed. Never rendered as an empty file.
    pub error: Option<String>,
}

/// A management action's mechanical result. `ok` reflects the runtime's own
/// store outcome, never a model claim.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct UiMemoryActionResult {
    /// `remember`, `dream`, `batch_dream`, `flush`, or `forget`.
    pub action: String,
    pub ok: bool,
    pub message: String,
}
