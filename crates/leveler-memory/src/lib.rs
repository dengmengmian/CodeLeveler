//! Project-scoped durable synthetic memory.
//!
//! - Writes are file-backed under a project memory root.
//! - Forget archives (does not hard-delete).
//! - Search is lexical BM25 over active entries only.
//! - INDEX is a short title list for cache-stable system injection; bodies stay
//!   out of the system prefix and are retrieved on demand.

#![forbid(unsafe_code)]

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// A durable memory entry.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MemoryEntry {
    pub id: String,
    pub title: String,
    pub body: String,
    pub tags: Vec<String>,
    pub created_at: String,
    pub updated_at: String,
    /// When set, the entry is archived (forgotten) and excluded from search.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub archived_at: Option<String>,
}

#[derive(Debug, thiserror::Error)]
pub enum MemoryError {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("serialize: {0}")]
    Serde(#[from] serde_json::Error),
    #[error("not found: {0}")]
    NotFound(String),
    #[error("invalid entry: {0}")]
    Invalid(String),
}

/// File-backed memory store under `root/{active,archive}/`.
#[derive(Debug, Clone)]
pub struct MemoryStore {
    root: PathBuf,
}

impl MemoryStore {
    pub fn open(root: impl Into<PathBuf>) -> Result<Self, MemoryError> {
        let root = root.into();
        fs::create_dir_all(root.join("active"))?;
        fs::create_dir_all(root.join("archive"))?;
        Ok(Self { root })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    fn active_path(&self, id: &str) -> PathBuf {
        self.root.join("active").join(format!("{id}.json"))
    }

    fn archive_path(&self, id: &str) -> PathBuf {
        self.root.join("archive").join(format!("{id}.json"))
    }

    /// Write or replace an active entry (caller handles approval).
    pub fn remember(&self, entry: MemoryEntry) -> Result<MemoryEntry, MemoryError> {
        if entry.id.trim().is_empty() || entry.title.trim().is_empty() {
            return Err(MemoryError::Invalid(
                "id and title are required".to_string(),
            ));
        }
        if entry.id.contains('/') || entry.id.contains('\\') || entry.id.contains("..") {
            return Err(MemoryError::Invalid("id must be a plain slug".to_string()));
        }
        let path = self.active_path(&entry.id);
        // Remove from archive if re-remembering.
        let _ = fs::remove_file(self.archive_path(&entry.id));
        let json = serde_json::to_string_pretty(&entry)?;
        fs::write(path, json)?;
        Ok(entry)
    }

    pub fn read_active(&self, id: &str) -> Result<MemoryEntry, MemoryError> {
        let path = self.active_path(id);
        if !path.exists() {
            return Err(MemoryError::NotFound(id.to_string()));
        }
        let raw = fs::read_to_string(path)?;
        Ok(serde_json::from_str(&raw)?)
    }

    /// Archive (forget) an active entry. Idempotent if already archived.
    pub fn forget(&self, id: &str) -> Result<MemoryEntry, MemoryError> {
        let active = self.active_path(id);
        let archive = self.archive_path(id);
        if active.exists() {
            let mut entry: MemoryEntry = serde_json::from_str(&fs::read_to_string(&active)?)?;
            entry.archived_at = Some(now_rfc3339());
            fs::write(&archive, serde_json::to_string_pretty(&entry)?)?;
            fs::remove_file(active)?;
            return Ok(entry);
        }
        if archive.exists() {
            let entry: MemoryEntry = serde_json::from_str(&fs::read_to_string(archive)?)?;
            return Ok(entry);
        }
        Err(MemoryError::NotFound(id.to_string()))
    }

    pub fn list_active(&self) -> Result<Vec<MemoryEntry>, MemoryError> {
        self.list_dir("active")
    }

    /// Archived (forgotten) entries for audit / doctor counts.
    pub fn list_archived(&self) -> Result<Vec<MemoryEntry>, MemoryError> {
        self.list_dir("archive")
    }

    /// `(active_count, archived_count)` for doctor / CLI status lines.
    pub fn counts(&self) -> Result<(usize, usize), MemoryError> {
        Ok((self.list_active()?.len(), self.list_archived()?.len()))
    }

    fn list_dir(&self, name: &str) -> Result<Vec<MemoryEntry>, MemoryError> {
        let dir = self.root.join(name);
        let mut out = Vec::new();
        if !dir.exists() {
            return Ok(out);
        }
        for entry in fs::read_dir(dir)? {
            let entry = entry?;
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("json") {
                continue;
            }
            let raw = fs::read_to_string(path)?;
            out.push(serde_json::from_str(&raw)?);
        }
        out.sort_by(|a, b| a.title.cmp(&b.title));
        Ok(out)
    }

    /// Short INDEX lines for system injection (titles only; cap lines).
    pub fn index_lines(&self, max_entries: usize) -> Result<String, MemoryError> {
        let entries = self.list_active()?;
        if entries.is_empty() {
            return Ok(String::new());
        }
        let mut lines = Vec::new();
        for (i, e) in entries.into_iter().take(max_entries).enumerate() {
            lines.push(format!("{}. [{}] {}", i + 1, e.id, e.title));
        }
        Ok(lines.join("\n"))
    }

    /// BM25-ish lexical search over active titles + bodies + tags.
    pub fn search(
        &self,
        query: &str,
        limit: usize,
    ) -> Result<Vec<(MemoryEntry, f64)>, MemoryError> {
        let q = tokenize(query);
        if q.is_empty() {
            return Ok(Vec::new());
        }
        let entries = self.list_active()?;
        let docs: Vec<Vec<String>> = entries
            .iter()
            .map(|e| tokenize(&format!("{} {} {}", e.title, e.body, e.tags.join(" "))))
            .collect();
        let n = docs.len() as f64;
        if n == 0.0 {
            return Ok(Vec::new());
        }
        let mut df: HashMap<&str, f64> = HashMap::new();
        for term in &q {
            let count = docs.iter().filter(|d| d.iter().any(|t| t == term)).count() as f64;
            df.insert(term.as_str(), count);
        }
        let avgdl = docs.iter().map(|d| d.len() as f64).sum::<f64>() / n;
        const K1: f64 = 1.2;
        const B: f64 = 0.75;
        let mut scored: Vec<(usize, f64)> = Vec::new();
        for (i, doc) in docs.iter().enumerate() {
            let dl = doc.len() as f64;
            let mut score = 0.0;
            for term in &q {
                let tf = doc.iter().filter(|t| *t == term).count() as f64;
                if tf == 0.0 {
                    continue;
                }
                let dfi = *df.get(term.as_str()).unwrap_or(&0.0);
                if dfi == 0.0 {
                    continue;
                }
                let idf = ((n - dfi + 0.5) / (dfi + 0.5) + 1.0).ln();
                let denom = tf + K1 * (1.0 - B + B * dl / avgdl.max(1.0));
                score += idf * (tf * (K1 + 1.0)) / denom;
            }
            if score > 0.0 {
                scored.push((i, score));
            }
        }
        scored.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
        Ok(scored
            .into_iter()
            .take(limit.max(1))
            .map(|(i, s)| (entries[i].clone(), s))
            .collect())
    }
}

/// Build a new entry with a slug id from title.
pub fn new_entry(title: &str, body: &str, tags: Vec<String>) -> MemoryEntry {
    let id = slugify(title);
    let ts = now_rfc3339();
    MemoryEntry {
        id,
        title: title.trim().to_string(),
        body: body.to_string(),
        tags,
        created_at: ts.clone(),
        updated_at: ts,
        archived_at: None,
    }
}

fn slugify(title: &str) -> String {
    let mut s: String = title
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() {
                c.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .collect();
    while s.contains("--") {
        s = s.replace("--", "-");
    }
    let s = s.trim_matches('-').to_string();
    if s.is_empty() {
        format!("mem-{}", now_rfc3339().replace([':', '+'], ""))
    } else {
        s.chars().take(48).collect()
    }
}

fn tokenize(text: &str) -> Vec<String> {
    text.to_lowercase()
        .split(|c: char| !c.is_alphanumeric())
        .filter(|t| t.len() > 1)
        .map(|t| t.to_string())
        .collect()
}

fn now_rfc3339() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn counts_track_active_and_archived() {
        let dir = tempdir().unwrap();
        let store = MemoryStore::open(dir.path()).unwrap();
        assert_eq!(store.counts().unwrap(), (0, 0));
        let e = new_entry(
            "prefer workspace write",
            "Use WorkspaceWrite by default.",
            vec![],
        );
        store.remember(e.clone()).unwrap();
        assert_eq!(store.counts().unwrap(), (1, 0));
        store.forget(&e.id).unwrap();
        assert_eq!(store.counts().unwrap(), (0, 1));
        assert_eq!(store.list_archived().unwrap().len(), 1);
    }

    #[test]
    fn remember_search_forget_archive() {
        let dir = tempdir().unwrap();
        let store = MemoryStore::open(dir.path()).unwrap();
        let entry = new_entry(
            "Use workspace write",
            "Prefer PermissionProfile::Assisted for edits.",
            vec!["policy".into()],
        );
        store.remember(entry.clone()).unwrap();
        let hits = store.search("workspace write", 5).unwrap();
        assert!(!hits.is_empty());
        assert_eq!(hits[0].0.id, entry.id);
        let idx = store.index_lines(10).unwrap();
        assert!(idx.contains(&entry.id));
        assert!(!idx.contains("Prefer PermissionProfile")); // body not in INDEX
        store.forget(&entry.id).unwrap();
        assert!(store.search("workspace", 5).unwrap().is_empty());
        assert!(store.archive_path(&entry.id).exists());
    }

    #[test]
    fn index_stable_without_body_leak() {
        let dir = tempdir().unwrap();
        let store = MemoryStore::open(dir.path()).unwrap();
        store
            .remember(new_entry("A", "secret body never in index", vec![]))
            .unwrap();
        let a = store.index_lines(20).unwrap();
        let b = store.index_lines(20).unwrap();
        assert_eq!(a, b);
        assert!(!a.contains("secret"));
    }
}

// ── Local "vector" retrieval (no embedding cloud; bag-of-hashes space) ─────

/// Deterministic local embedding: 256-d bag of hashed tokens in [-1,1].
/// Not a neural embedding — a model-agnostic dense vector for cosine search.
pub fn local_embed(text: &str) -> Vec<f32> {
    const DIM: usize = 256;
    let mut v = vec![0.0f32; DIM];
    for tok in tokenize(text) {
        let mut h: u64 = 0xcbf29ce484222325;
        for b in tok.bytes() {
            h ^= b as u64;
            h = h.wrapping_mul(0x100000001b3);
        }
        let idx = (h as usize) % DIM;
        let sign = if h & 1 == 0 { 1.0 } else { -1.0 };
        v[idx] += sign;
    }
    // L2 normalize
    let norm = v.iter().map(|x| x * x).sum::<f32>().sqrt().max(1e-9);
    for x in &mut v {
        *x /= norm;
    }
    v
}

pub fn cosine(a: &[f32], b: &[f32]) -> f32 {
    a.iter().zip(b.iter()).map(|(x, y)| x * y).sum()
}

impl MemoryStore {
    /// Dense local-vector search (cosine over [`local_embed`]).
    pub fn vector_search(
        &self,
        query: &str,
        limit: usize,
    ) -> Result<Vec<(MemoryEntry, f64)>, MemoryError> {
        let q = local_embed(query);
        let entries = self.list_active()?;
        let mut scored: Vec<(MemoryEntry, f64)> = entries
            .into_iter()
            .map(|e| {
                let doc = local_embed(&format!("{} {} {}", e.title, e.body, e.tags.join(" ")));
                let score = cosine(&q, &doc) as f64;
                (e, score)
            })
            .filter(|(_, s)| *s > 0.05)
            .collect();
        scored.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
        Ok(scored.into_iter().take(limit.max(1)).collect())
    }
}

/// Extract durable-looking candidate memories from a session transcript text.
/// Does **not** write — caller must approve (or use auto_consolidate after user opt-in).
pub fn extract_memory_candidates(transcript: &str, max: usize) -> Vec<MemoryEntry> {
    let mut out = Vec::new();
    // Heuristic lines: "prefer …", "always …", "约定：", "决策：", bullet conclusions.
    for line in transcript.lines() {
        let t = line.trim();
        if t.len() < 12 || t.len() > 400 {
            continue;
        }
        let lower = t.to_lowercase();
        let hit = [
            "prefer ",
            "always ",
            "never ",
            "use ",
            "约定",
            "决策",
            "preference",
            "we decided",
            "must ",
        ]
        .iter()
        .any(|k| lower.contains(k));
        if !hit {
            continue;
        }
        let title: String = t.chars().take(80).collect();
        out.push(new_entry(&title, t, vec!["session-extract".into()]));
        if out.len() >= max {
            break;
        }
    }
    out
}

#[cfg(test)]
mod vector_tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn vector_search_ranks_related_entry() {
        let dir = tempdir().unwrap();
        let store = MemoryStore::open(dir.path()).unwrap();
        store
            .remember(new_entry(
                "Workspace write mode",
                "Prefer PermissionProfile::Assisted for local edits.",
                vec![],
            ))
            .unwrap();
        store
            .remember(new_entry(
                "Cooking pasta",
                "Boil water then add salt.",
                vec![],
            ))
            .unwrap();
        let hits = store.vector_search("workspace write edits", 3).unwrap();
        assert!(!hits.is_empty());
        assert!(
            hits[0].0.title.to_lowercase().contains("workspace"),
            "{:?}",
            hits[0].0.title
        );
    }

    #[test]
    fn extract_candidates_finds_preference_lines() {
        let t = "hello\nWe decided to prefer workspace-write for agent edits.\nother\n";
        let c = extract_memory_candidates(t, 5);
        assert!(!c.is_empty());
        assert!(c[0].body.contains("prefer"));
    }
}
