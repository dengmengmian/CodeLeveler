//! Ordinary file tools meeting the host memory policy.
//!
//! Memory files live outside the workspace but are not ordinary external
//! files: the host owns containment, concurrency, and the generated manifest.
//! Every tool that can touch them asks this adapter first. A path outside the
//! configured roots keeps its normal behavior; a path inside them is validated
//! or persisted by the policy, never by the workspace editor.

use std::path::Path;

use leveler_memory::{MemoryV2AccessResource, MemoryV2Write};

use crate::ToolContext;

fn resource(context: &ToolContext) -> Option<MemoryV2AccessResource> {
    context.execution.memory.clone()
}

/// Whether `path` belongs to memory. `Err` means the policy refused it
/// (excluded, escaping, or symlinked); callers surface that as a tool error.
pub(crate) fn validate_read(context: &ToolContext, path: &Path) -> Result<bool, String> {
    let Some(memory) = resource(context) else {
        return Ok(false);
    };
    if !path.is_absolute() {
        return Ok(false);
    }
    memory.0.validate_read(path)
}

/// Record the exact bytes an ordinary read observed, so a later edit can
/// detect an intervening change.
pub(crate) async fn record_read(
    context: &ToolContext,
    path: &Path,
    contents: Vec<u8>,
) -> Result<(), String> {
    let Some(memory) = resource(context) else {
        return Ok(());
    };
    if !path.is_absolute() {
        return Ok(());
    }
    let path = path.to_path_buf();
    tokio::task::spawn_blocking(move || memory.0.record_read(&path, &contents))
        .await
        .map_err(|error| format!("memory read snapshot task failed: {error}"))?
}

/// Validate a create/replace without persisting it. `Ok(true)` means the
/// policy owns the path; `Ok(false)` means ordinary file behavior applies.
pub(crate) async fn preflight_write(
    context: &ToolContext,
    path: &Path,
    contents: &[u8],
) -> Result<bool, String> {
    let Some(memory) = resource(context) else {
        return Ok(false);
    };
    if !path.is_absolute() {
        return Ok(false);
    }
    let path = path.to_path_buf();
    let contents = contents.to_vec();
    tokio::task::spawn_blocking(move || memory.0.preflight_write(&path, &contents))
        .await
        .map_err(|error| format!("memory write preflight task failed: {error}"))?
}

/// Persist a memory file through the policy. Only call after
/// [`preflight_write`] returned `Ok(true)`.
pub(crate) async fn write_file(
    context: &ToolContext,
    path: &Path,
    contents: &[u8],
) -> Result<MemoryV2Write, String> {
    let Some(memory) = resource(context) else {
        return Ok(MemoryV2Write::Outside);
    };
    let path = path.to_path_buf();
    let contents = contents.to_vec();
    tokio::task::spawn_blocking(move || memory.0.write_file(&path, &contents))
        .await
        .map_err(|error| format!("memory write task failed: {error}"))?
}

/// Approval is not the authority for memory files: the policy is. A path the
/// policy recognizes must not produce a workspace-scope consent prompt, and a
/// path it refuses fails at execution with the policy's own reason.
pub(crate) fn owns_path(context: &ToolContext, path: &str) -> bool {
    let path = Path::new(path);
    if !path.is_absolute() {
        return false;
    }
    validate_read(context, path).unwrap_or(false)
}
