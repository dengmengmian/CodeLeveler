//! Project-scoped remembered permission grants (SEC-2) — **legacy store**.
//!
//! Stores only action signatures (not secrets). `ApproveSession` used to
//! append here; durable standing permission is now expressed as SEC-1
//! permission rules (`.leveler/permissions.yaml`) written on `ApproveAlways`,
//! and `ApproveSession` is strictly session-scoped. The file is still LOADED
//! at drive start so grants remembered by older versions keep working, but it
//! is no longer written.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::approval::is_memory_write_tool;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GrantRecord {
    pub signature: String,
    /// Always `"project"` for durable entries in this store.
    pub scope: String,
    pub created_at: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct GrantFile {
    #[serde(default)]
    pub grants: Vec<GrantRecord>,
}

/// Path of the grant file under a project state directory.
pub fn grants_path(state_dir: &Path) -> PathBuf {
    state_dir.join("permission_grants.json")
}

pub fn load_grants(state_dir: &Path) -> GrantFile {
    let path = grants_path(state_dir);
    let Ok(raw) = std::fs::read_to_string(&path) else {
        return GrantFile::default();
    };
    serde_json::from_str(&raw).unwrap_or_default()
}

pub fn signatures_from_file(file: &GrantFile) -> BTreeSet<String> {
    file.grants.iter().map(|g| g.signature.clone()).collect()
}

/// Persist a session-approved signature as a project grant.
/// Memory-write tools are never remembered.
pub fn remember_project_grant(
    state_dir: &Path,
    signature: &str,
    tool: &str,
    now_iso: &str,
) -> Result<(), String> {
    if is_memory_write_tool(tool) {
        return Ok(());
    }
    if signature.is_empty() {
        return Ok(());
    }
    std::fs::create_dir_all(state_dir).map_err(|e| format!("create state_dir: {e}"))?;
    let path = grants_path(state_dir);
    let mut file = load_grants(state_dir);
    if file.grants.iter().any(|g| g.signature == signature) {
        return Ok(());
    }
    file.grants.push(GrantRecord {
        signature: signature.to_string(),
        scope: "project".into(),
        created_at: now_iso.to_string(),
    });
    let raw = serde_json::to_string_pretty(&file).map_err(|e| e.to_string())?;
    std::fs::write(&path, raw).map_err(|e| format!("write {}: {e}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_and_skip_memory_tools() {
        let dir = tempfile::tempdir().unwrap();
        remember_project_grant(
            dir.path(),
            "sig:run_command:cargo test",
            "run_command",
            "t0",
        )
        .unwrap();
        remember_project_grant(dir.path(), "sig:remember:x", "remember", "t1").unwrap();
        let file = load_grants(dir.path());
        assert_eq!(file.grants.len(), 1);
        assert_eq!(file.grants[0].signature, "sig:run_command:cargo test");
        let set = signatures_from_file(&file);
        assert!(set.contains("sig:run_command:cargo test"));
    }
}
