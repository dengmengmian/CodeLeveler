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
use std::io::Write;
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
        validate_entry(&entry)?;
        let path = self.active_path(&entry.id);
        let json = serde_json::to_string_pretty(&entry)?;
        write_atomically(&path, json.as_bytes())?;
        // Remove from archive only after the active copy is durable.
        let _ = fs::remove_file(self.archive_path(&entry.id));
        Ok(entry)
    }

    /// Atomically choose a non-conflicting slug and store a complete entry.
    ///
    /// The no-overwrite hard link is the reservation: concurrent writers can
    /// never both claim the same id, and readers never observe a partial JSON
    /// file. An unreadable existing entry is an error, not evidence that the id
    /// is available.
    pub fn remember_deduplicated(
        &self,
        mut entry: MemoryEntry,
    ) -> Result<MemoryEntry, MemoryError> {
        validate_entry(&entry)?;
        let base = entry.id.clone();
        let mut suffix = 1usize;
        loop {
            entry.id = if suffix == 1 {
                base.clone()
            } else {
                format!("{base}-{suffix}")
            };
            let path = self.active_path(&entry.id);
            let json = serde_json::to_string_pretty(&entry)?;
            let mut temp = tempfile::Builder::new()
                .prefix(".memory-")
                .tempfile_in(self.root.join("active"))?;
            temp.write_all(json.as_bytes())?;
            temp.as_file().sync_all()?;

            match fs::hard_link(temp.path(), &path) {
                Ok(()) => {
                    let _ = fs::remove_file(self.archive_path(&entry.id));
                    return Ok(entry);
                }
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                    match self.read_active(&entry.id) {
                        Ok(existing)
                            if existing.title == entry.title
                                && existing.body.trim() == entry.body.trim() =>
                        {
                            return Ok(existing);
                        }
                        Ok(_) => {
                            suffix = suffix.checked_add(1).ok_or_else(|| {
                                MemoryError::Invalid("too many colliding memory ids".to_string())
                            })?;
                        }
                        Err(MemoryError::NotFound(_)) => continue,
                        Err(error) => return Err(error),
                    }
                }
                Err(error) => return Err(MemoryError::Io(error)),
            }
        }
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

fn validate_entry(entry: &MemoryEntry) -> Result<(), MemoryError> {
    if entry.id.trim().is_empty() || entry.title.trim().is_empty() {
        return Err(MemoryError::Invalid(
            "id and title are required".to_string(),
        ));
    }
    if entry.id.contains('/') || entry.id.contains('\\') || entry.id.contains("..") {
        return Err(MemoryError::Invalid("id must be a plain slug".to_string()));
    }
    Ok(())
}

fn write_atomically(path: &Path, bytes: &[u8]) -> Result<(), MemoryError> {
    let parent = path
        .parent()
        .ok_or_else(|| MemoryError::Invalid("memory path has no parent".to_string()))?;
    let mut temp = tempfile::Builder::new()
        .prefix(".memory-")
        .tempfile_in(parent)?;
    temp.write_all(bytes)?;
    temp.as_file().sync_all()?;
    temp.persist(path)
        .map(|_| ())
        .map_err(|error| MemoryError::Io(error.error))
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

/// True for ideographic / kana / hangul scripts that write without spaces.
fn is_cjk(c: char) -> bool {
    matches!(c as u32,
        0x3400..=0x9FFF   // CJK unified (incl. ext A)
        | 0xF900..=0xFAFF // CJK compatibility ideographs
        | 0x3040..=0x30FF // hiragana + katakana
        | 0xAC00..=0xD7A3 // hangul syllables
    )
}

/// Split text into lexical terms for BM25. ASCII/alphanumeric runs become
/// whole words (as before); CJK runs — which have no spaces — become overlapping
/// character bigrams so Chinese queries actually match. A lone CJK char falls
/// back to a unigram. Without this, `is_alphanumeric()` treats a whole Chinese
/// phrase as one token and search never matches (recall + `/memory` both broke).
fn tokenize(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut word = String::new();
    let mut cjk: Vec<char> = Vec::new();

    fn flush_word(word: &mut String, out: &mut Vec<String>) {
        if word.chars().count() > 1 {
            out.push(std::mem::take(word));
        } else {
            word.clear();
        }
    }
    fn flush_cjk(cjk: &mut Vec<char>, out: &mut Vec<String>) {
        match cjk.len() {
            0 => {}
            1 => out.push(cjk[0].to_string()),
            _ => {
                for w in cjk.windows(2) {
                    out.push(w.iter().collect());
                }
            }
        }
        cjk.clear();
    }

    for c in text.chars() {
        if is_cjk(c) {
            flush_word(&mut word, &mut out);
            cjk.push(c);
        } else if c.is_alphanumeric() {
            flush_cjk(&mut cjk, &mut out);
            word.extend(c.to_lowercase());
        } else {
            flush_word(&mut word, &mut out);
            flush_cjk(&mut cjk, &mut out);
        }
    }
    flush_word(&mut word, &mut out);
    flush_cjk(&mut cjk, &mut out);
    out
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
    fn tokenize_bigrams_chinese_and_keeps_ascii_words() {
        let t = tokenize("部署密钥 Zephyr-Q7");
        // Overlapping CJK bigrams…
        assert!(t.contains(&"部署".to_string()), "{t:?}");
        assert!(t.contains(&"署密".to_string()), "{t:?}");
        assert!(t.contains(&"密钥".to_string()), "{t:?}");
        // …plus ASCII runs as lowercased words.
        assert!(t.contains(&"zephyr".to_string()), "{t:?}");
        assert!(t.contains(&"q7".to_string()), "{t:?}");
    }

    #[test]
    fn search_matches_chinese_query() {
        let dir = tempdir().unwrap();
        let store = MemoryStore::open(dir.path()).unwrap();
        let entry = new_entry("部署密钥保管人", "由代号 Zephyr-Q7 的同事保管。", vec![]);
        store.remember(entry.clone()).unwrap();
        // A Chinese query must retrieve the entry (regression: whole-phrase token
        // never matched before CJK bigram tokenization).
        let hits = store.search("谁保管部署密钥", 5).unwrap();
        assert!(!hits.is_empty(), "chinese search returned nothing");
        assert_eq!(hits[0].0.id, entry.id);
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
    fn concurrent_deduplicated_remembers_never_clobber_each_other() {
        let dir = tempdir().unwrap();
        let store = MemoryStore::open(dir.path()).unwrap();
        let gate = std::sync::Arc::new(std::sync::Barrier::new(9));
        let mut threads = Vec::new();
        for index in 0..8 {
            let store = store.clone();
            let gate = gate.clone();
            threads.push(std::thread::spawn(move || {
                let entry = new_entry("Deploy notes", &format!("fact {index}"), vec![]);
                gate.wait();
                store.remember_deduplicated(entry).unwrap()
            }));
        }
        gate.wait();
        let saved: Vec<_> = threads
            .into_iter()
            .map(|thread| thread.join().unwrap())
            .collect();
        let ids: std::collections::HashSet<_> =
            saved.iter().map(|entry| entry.id.as_str()).collect();
        assert_eq!(ids.len(), 8);
        assert_eq!(store.list_active().unwrap().len(), 8);
    }

    #[test]
    fn deduplication_does_not_treat_a_corrupt_entry_as_a_free_id() {
        let dir = tempdir().unwrap();
        let store = MemoryStore::open(dir.path()).unwrap();
        std::fs::write(store.active_path("deploy-notes"), "{not-json").unwrap();

        let error = store
            .remember_deduplicated(new_entry("Deploy notes", "fact", vec![]))
            .unwrap_err();
        assert!(matches!(error, MemoryError::Serde(_)), "{error}");
        assert!(!store.active_path("deploy-notes-2").exists());
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
