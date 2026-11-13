//! Host-provided policy hook for ordinary file tools accessing memory roots.
//!
//! The tools layer owns only this narrow interface. The memory crate implements
//! containment, optimistic concurrency, atomic writes, and manifest refreshes.

use std::path::{Path, PathBuf};
use std::sync::Arc;

/// Result of routing a write through the memory policy.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MemoryWrite {
    /// The path is outside configured memory roots; preserve normal tool behavior.
    Outside,
    /// The policy completed the write and refreshed the generated manifest.
    Written { previous_content: Option<Vec<u8>> },
}

/// Session-scoped policy implemented by the memory storage layer.
pub trait MemoryAccess: std::fmt::Debug + Send + Sync {
    /// Validate a read/search/list target. Returns whether it belongs to memory.
    fn validate_read(&self, path: &Path) -> Result<bool, String>;

    /// Record the full content observed by a successful ordinary file read.
    fn record_read(&self, path: &Path, contents: &[u8]) -> Result<(), String>;

    /// Validate a create/replace without persisting it.
    fn preflight_write(&self, path: &Path, contents: &[u8]) -> Result<bool, String>;

    /// Atomically create or replace a permitted memory file.
    fn write_file(&self, path: &Path, contents: &[u8]) -> Result<MemoryWrite, String>;

    /// Return both configured scope roots for prompt and UI metadata.
    fn scope_roots(&self) -> [PathBuf; 2];
}

/// Shared handle installed in the tool execution resources.
#[derive(Clone)]
pub struct MemoryAccessResource(pub Arc<dyn MemoryAccess>);

impl std::fmt::Debug for MemoryAccessResource {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.debug_struct("MemoryAccessResource").finish()
    }
}
