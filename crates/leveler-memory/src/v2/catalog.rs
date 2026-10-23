//! Catalog state for the cache-stable memory prefix.
//!
//! The catalog is derived from Markdown current records. Its version and
//! digest live beside the index under the private state root; deleting them
//! only forces the next run to rebuild. No catalog file is a content authority.

use std::path::Path;

use serde::{Deserialize, Serialize};

use super::frontmatter::sha256_hex_text;
use super::path::{PathError, atomic_write};
use super::record::MemoryScope;

pub const CATALOG_SCHEMA: u32 = 1;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CatalogEntry {
    pub id: String,
    pub revision: u64,
    /// Title is included only while the record is current and non-sensitive.
    pub title: String,
    pub status: String,
    pub pinned_by_user: bool,
    pub authority: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CatalogState {
    pub schema: u32,
    pub version: u64,
    pub digest: String,
    pub entries: Vec<CatalogEntry>,
}

impl Default for CatalogState {
    fn default() -> Self {
        Self {
            schema: CATALOG_SCHEMA,
            version: 0,
            digest: empty_digest(),
            entries: Vec::new(),
        }
    }
}

/// A change between the injected catalog and current Markdown.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CatalogChange {
    pub id: String,
    pub revision: u64,
    /// `added`, `updated` or `removed`.
    pub operation: String,
    /// Never populated for `removed`: a forgotten title must not reappear in a
    /// tail delta.
    pub title: Option<String>,
    pub scope: MemoryScope,
}

pub struct CatalogUpdate {
    pub state: CatalogState,
    pub changes: Vec<CatalogChange>,
}

/// Load a scope's catalog snapshot. A malformed file is treated as absent:
/// the state is a derived cache, not a fact.
pub fn load(state_root: &Path, scope: MemoryScope) -> Option<CatalogState> {
    let path = state_root.join("catalog").join(catalog_file(scope));
    let bytes = std::fs::read(path).ok()?;
    let state: CatalogState = serde_json::from_slice(&bytes).ok()?;
    (state.schema == CATALOG_SCHEMA).then_some(state)
}

/// Record the current catalog and return the monotonic version plus changes.
///
/// This is called only by the memory context builder, the single catalog
/// owner. The write is atomic and contains no memory body.
pub fn record(
    state_root: &Path,
    scope: MemoryScope,
    entries: Vec<CatalogEntry>,
) -> Result<CatalogUpdate, PathError> {
    let mut entries = entries;
    entries.sort_by(|left, right| left.id.cmp(&right.id));
    let digest = digest_of(&entries);
    let previous = load(state_root, scope).unwrap_or_default();
    if previous.digest == digest {
        return Ok(CatalogUpdate {
            state: CatalogState {
                schema: CATALOG_SCHEMA,
                version: previous.version,
                digest,
                entries,
            },
            changes: Vec::new(),
        });
    }
    let changes = diff(&previous.entries, &entries, scope);
    let state = CatalogState {
        schema: CATALOG_SCHEMA,
        version: previous.version.saturating_add(1),
        digest,
        entries,
    };
    let directory = state_root.join("catalog");
    std::fs::create_dir_all(&directory).map_err(|error| PathError::Io {
        path: directory.clone(),
        source: error,
    })?;
    let bytes = serde_json::to_vec_pretty(&state).map_err(|error| PathError::Io {
        path: directory.clone(),
        source: std::io::Error::other(error),
    })?;
    atomic_write(&directory.join(catalog_file(scope)), &bytes)?;
    Ok(CatalogUpdate { state, changes })
}

/// Digest of the rendered catalog inputs, sorted by id.
pub fn digest_of(entries: &[CatalogEntry]) -> String {
    let mut sorted = entries.to_vec();
    sorted.sort_by(|left, right| left.id.cmp(&right.id));
    match serde_json::to_string(&sorted) {
        Ok(rendered) => sha256_hex_text(&rendered),
        Err(_) => empty_digest(),
    }
}

fn empty_digest() -> String {
    sha256_hex_text("[]")
}

fn diff(
    previous: &[CatalogEntry],
    current: &[CatalogEntry],
    scope: MemoryScope,
) -> Vec<CatalogChange> {
    let mut changes = Vec::new();
    for entry in current {
        match previous.iter().find(|old| old.id == entry.id) {
            None => changes.push(CatalogChange {
                id: entry.id.clone(),
                revision: entry.revision,
                operation: "added".into(),
                title: Some(entry.title.clone()),
                scope,
            }),
            Some(old)
                if old.revision != entry.revision
                    || old.title != entry.title
                    || old.status != entry.status
                    || old.pinned_by_user != entry.pinned_by_user =>
            {
                changes.push(CatalogChange {
                    id: entry.id.clone(),
                    revision: entry.revision,
                    operation: "updated".into(),
                    title: Some(entry.title.clone()),
                    scope,
                });
            }
            Some(_) => {}
        }
    }
    for old in previous {
        if !current.iter().any(|entry| entry.id == old.id) {
            changes.push(CatalogChange {
                id: old.id.clone(),
                revision: old.revision,
                operation: "removed".into(),
                title: None,
                scope,
            });
        }
    }
    changes
}

fn catalog_file(scope: MemoryScope) -> &'static str {
    match scope {
        MemoryScope::Global => "global.json",
        MemoryScope::Project => "project.json",
        MemoryScope::Local => "local.json",
    }
}
