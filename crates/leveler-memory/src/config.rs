//! Memory configuration value types shared by the storage, capture, and Dream
//! pipelines.
//!
//! These are resolved leaf values. Raw TOML parsing lives in the application's
//! config layer so this crate keeps one authority for what a running session
//! may do.

use serde::{Deserialize, Serialize};

/// Index and chunking configuration.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(default)]
pub struct MemoryIndexConfig {
    /// Maximum chunk size in characters (about 4 characters per token).
    pub max_chunk_chars: usize,
    /// Character overlap between consecutive chunks.
    pub chunk_overlap_chars: usize,
}

impl Default for MemoryIndexConfig {
    fn default() -> Self {
        Self {
            max_chunk_chars: 1600,
            chunk_overlap_chars: 320,
        }
    }
}

/// How much of the memory pipeline a session is allowed to run.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MemoryV2Rollout {
    /// Disable every memory read, capture, Dream, and write path.
    #[default]
    Off,
    /// Persist extracted observations without exposing them in manifests or Dream.
    RecordOnly,
    /// Evaluate capture and Dream plans without committing curated topic changes.
    Shadow,
    /// Enable the complete pipeline.
    Active,
}

impl MemoryV2Rollout {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::RecordOnly => "record_only",
            Self::Shadow => "shadow",
            Self::Active => "active",
        }
    }

    pub fn allows_capture(self) -> bool {
        self != Self::Off
    }

    pub fn exposes_memory(self) -> bool {
        self == Self::Active
    }

    pub fn commits_topics(self) -> bool {
        self == Self::Active
    }

    /// The more restrictive of two rollout values.
    pub fn restrict(self, other: Self) -> Self {
        fn rank(value: MemoryV2Rollout) -> u8 {
            match value {
                MemoryV2Rollout::Off => 0,
                MemoryV2Rollout::RecordOnly => 1,
                MemoryV2Rollout::Shadow => 2,
                MemoryV2Rollout::Active => 3,
            }
        }
        if rank(self) <= rank(other) {
            self
        } else {
            other
        }
    }
}

/// Concrete memory controls pinned when a session starts.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(default)]
pub struct MemoryV2Config {
    pub rollout: MemoryV2Rollout,
    pub capture_enabled: bool,
    pub automatic_dream_enabled: bool,
    pub manual_dream_enabled: bool,
    pub batch_dream_enabled: bool,
    pub batch_dream_max_run_secs: u64,
    pub batch_dream_max_calls_per_batch: usize,
    pub batch_dream_max_batch_note_bytes: usize,
    pub compact_index_enabled: bool,
    pub file_writes_enabled: bool,
    pub archived_retention_days: u64,
    pub job_retention_days: u64,
}

impl Default for MemoryV2Config {
    fn default() -> Self {
        Self {
            rollout: MemoryV2Rollout::Off,
            capture_enabled: true,
            automatic_dream_enabled: true,
            manual_dream_enabled: true,
            batch_dream_enabled: false,
            batch_dream_max_run_secs: 1_800,
            batch_dream_max_calls_per_batch: 6,
            batch_dream_max_batch_note_bytes: 96 * 1024,
            compact_index_enabled: false,
            file_writes_enabled: true,
            archived_retention_days: 30,
            job_retention_days: 14,
        }
    }
}

impl MemoryV2Config {
    pub fn can_capture(self) -> bool {
        self.rollout.allows_capture() && self.capture_enabled && self.file_writes_enabled
    }

    pub fn can_run_maintenance(self) -> bool {
        self.rollout.allows_capture() && self.file_writes_enabled
    }

    pub fn can_run_automatic_dream(self) -> bool {
        matches!(
            self.rollout,
            MemoryV2Rollout::Shadow | MemoryV2Rollout::Active
        ) && self.automatic_dream_enabled
            && self.file_writes_enabled
    }

    pub fn can_run_manual_dream(self) -> bool {
        matches!(
            self.rollout,
            MemoryV2Rollout::Shadow | MemoryV2Rollout::Active
        ) && self.manual_dream_enabled
            && self.file_writes_enabled
    }

    pub fn can_expose_memory(self) -> bool {
        self.rollout.exposes_memory()
    }
}
