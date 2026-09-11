//! Project-scoped durable synthetic memory.
//!
//! - Writes are file-backed under a project memory root.
//! - Forget archives (does not hard-delete).
//! - Search is lexical BM25 over active entries only.
//! - INDEX is a short title list for cache-stable system injection; bodies stay
//!   out of the system prefix and are retrieved on demand.

#![forbid(unsafe_code)]

mod candidates;
mod pipeline;

pub use candidates::{
    CandidateKind, CandidateSource, MemoryCandidate, fingerprint_of, looks_like_secret,
    package_manager_from_root, parse_direct_memory_command, parse_inferred_preference,
    title_from_body,
};
pub use pipeline::{ProposeOutcome, SuppressRecord, collect_turn_candidates};

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
    /// Optional structured key (e.g. `package_manager`) for upserts / soft-hints.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub key: Option<String>,
    /// Free-form kind label (`preference`, `package_manager`, …).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
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

/// File-backed memory store under `root/{active,archive,pending,suppress}/`.
///
/// - `active/` — user-accepted durable memories
/// - `archive/` — forgotten entries
/// - `pending/` — system/user candidates awaiting consent ([`Self::accept`])
/// - `suppress/` — fingerprints rejected so the same signal does not re-spam
#[derive(Debug, Clone)]
pub struct MemoryStore {
    root: PathBuf,
}

impl MemoryStore {
    pub fn open(root: impl Into<PathBuf>) -> Result<Self, MemoryError> {
        let root = root.into();
        fs::create_dir_all(root.join("active"))?;
        fs::create_dir_all(root.join("archive"))?;
        fs::create_dir_all(root.join("pending"))?;
        fs::create_dir_all(root.join("suppress"))?;
        Ok(Self { root })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub(crate) fn active_path(&self, id: &str) -> PathBuf {
        self.root.join("active").join(format!("{id}.json"))
    }

    pub(crate) fn archive_path(&self, id: &str) -> PathBuf {
        self.root.join("archive").join(format!("{id}.json"))
    }

    pub(crate) fn pending_path(&self, id: &str) -> PathBuf {
        self.root.join("pending").join(format!("{id}.json"))
    }

    pub(crate) fn suppress_path(&self, fingerprint: &str) -> PathBuf {
        // Fingerprints are hex; still sanitize path segments.
        let safe: String = fingerprint
            .chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                    c
                } else {
                    '_'
                }
            })
            .collect();
        self.root.join("suppress").join(format!("{safe}.json"))
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
    /// THE way a new active memory comes into being.
    ///
    /// Every caller that creates active memory goes through here — `/remember`,
    /// the Web panel, the CLI, an approved agent proposal, and accepting a
    /// candidate. Before this existed each one picked between `remember`
    /// (upsert by id) and `remember_deduplicated` and decided overwrite,
    /// dedup and pending cleanup for itself; the CLI picked the upsert and
    /// silently destroyed same-titled notes.
    ///
    /// Guarantees: never overwrites; identical title+body returns the
    /// entry that already exists (so a retry is idempotent); the same title
    /// with different content gets its own id; secrets are refused here rather
    /// than in each UI; and an equivalent pending candidate is cleaned up once
    /// the active copy is durable — a failure to clean up leaves the active
    /// entry in place rather than rolling it back.
    pub fn activate(
        &self,
        title: &str,
        body: &str,
        kind: MemoryKind,
        tags: Vec<String>,
    ) -> Result<MemoryEntry, MemoryError> {
        let mut entry = new_entry(title, body, tags);
        entry.kind = Some(kind.as_str().to_string());
        // `remember_deduplicated` already owns the hard parts: a hard-link
        // reservation so concurrent writers cannot claim one id, an identical
        // title+body returning what is there, and a `-N` suffix otherwise.
        // `validate_entry` inside it is where a credential is refused.
        let saved = self.remember_deduplicated(entry)?;
        // Best effort, deliberately after the active copy is durable: a
        // candidate left behind would ask the user to approve what is already
        // stored, but failing to remove it is not a reason to lose the write.
        // The domain deliberately takes no logger; a failure here is healed by
        // `list_pending`, which drops covered candidates on the next read.
        let _ = self.clear_equivalent_pending(&saved);
        Ok(saved)
    }

    /// Drop pending candidates whose content this active entry now covers.
    ///
    /// Also run from [`Self::list_pending`], so a crash between the two steps
    /// heals on the next read instead of leaving a permanent duplicate.
    fn clear_equivalent_pending(&self, active: &MemoryEntry) -> Result<(), MemoryError> {
        for candidate in self.list_pending_raw()? {
            if candidate.body.trim() == active.body.trim() {
                let _ = fs::remove_file(self.pending_path(&candidate.id));
            }
        }
        Ok(())
    }

    /// Titles the model may ask about but which are NOT injected every turn.
    ///
    /// Replaces the old "all active titles" index. Its one job is discovery:
    /// when query recall misses on wording, a decision or note can still be
    /// found by title and read with the `memory` tool. Preferences are excluded
    /// because they are already injected in full, derived and sensitive entries
    /// because they must never reach the model this way.
    pub fn catalog_lines(&self, max_entries: usize) -> Result<String, MemoryError> {
        let mut entries: Vec<MemoryEntry> = self
            .list_active()?
            .into_iter()
            .filter(|e| {
                !is_sensitive(e)
                    && matches!(
                        MemoryKind::of(e),
                        MemoryKind::Decision | MemoryKind::Note | MemoryKind::LegacyUnknown
                    )
            })
            .collect();
        // Newest first: a cap that dropped the most recent decisions would
        // hide exactly the ones a turn is most likely to need.
        entries.sort_by(|a, b| {
            b.updated_at
                .cmp(&a.updated_at)
                .then_with(|| b.created_at.cmp(&a.created_at))
                .then_with(|| a.id.cmp(&b.id))
        });
        entries.truncate(max_entries);
        Ok(entries
            .into_iter()
            .enumerate()
            .map(|(i, e)| format!("{}. [{}] {}", i + 1, e.id, e.title))
            .collect::<Vec<_>>()
            .join("\n"))
    }

    /// Active entries that are LASTING PREFERENCES, for unconditional
    /// injection.
    ///
    /// A preference's relevance never depended on this turn's wording, so
    /// gating it behind lexical overlap is the wrong shape: a saved
    /// "keep the terminal output compact" is just as true when the user types
    /// "能不能精简一点", which shares no characters with it and scores zero.
    ///
    /// Only entries that SAY they are preferences qualify — `kind` or a
    /// `preference` tag. An entry with no kind is left to query recall rather
    /// than promoted on a guess, because older stores predate the label.
    /// Repository-derived facts never qualify. Order is by `created_at` then
    /// `id` so the block is deterministic across runs.
    pub fn standing_preferences(&self, limit: usize) -> Result<Vec<MemoryEntry>, MemoryError> {
        let mut out: Vec<MemoryEntry> = self
            .list_active()?
            .into_iter()
            .filter(|e| is_standing_preference(e) && !is_sensitive(e))
            .collect();
        out.sort_by(|a, b| {
            a.created_at
                .cmp(&b.created_at)
                .then_with(|| a.id.cmp(&b.id))
        });
        out.truncate(limit);
        Ok(out)
    }

    /// Retrieval for AUTOMATIC injection: `search`, minus repository-derived
    /// facts.
    ///
    /// A derived fact (the package manager, say) is readable from the
    /// repository itself, so a stored copy is a second source of truth that
    /// goes stale the moment the project switches tools. Existing entries are
    /// NOT deleted — a user's active data is theirs — they simply stop being
    /// injected behind their back. `search` still returns them so `/memory`
    /// and `doctor` can show what is there.
    pub fn recall(
        &self,
        query: &str,
        limit: usize,
    ) -> Result<Vec<(MemoryEntry, f64)>, MemoryError> {
        let mut hits = self.search(query, limit.saturating_add(DERIVED_RECALL_SLACK))?;
        hits.retain(|(entry, _)| !is_derived_fact(entry) && !is_sensitive(entry));
        hits.truncate(limit);
        Ok(hits)
    }

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
    // The ONE place a credential is refused. Putting it in each UI, CLI and
    // tool would mean four copies of the rule and four chances to forget it.
    // Entries already on disk are not touched here — they are withheld from
    // every automatic path instead (see `is_sensitive`).
    if looks_like_secret(&entry.title) || looks_like_secret(&entry.body) {
        return Err(MemoryError::Invalid(
            "refusing to store what looks like a credential".to_string(),
        ));
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
        key: None,
        kind: None,
    }
}

/// Build an active entry from an accepted candidate (preserves key/kind tags).
pub fn entry_from_candidate(candidate: &MemoryCandidate) -> MemoryEntry {
    let mut entry = new_entry(&candidate.title, &candidate.body, candidate.tags.clone());
    // Prefer stable id from key when present so re-accept upserts cleanly.
    if let Some(key) = &candidate.key {
        entry.id = slugify(key);
        entry.key = Some(key.clone());
    }
    entry.kind = Some(
        match candidate.kind {
            CandidateKind::Preference => "preference",
            CandidateKind::PackageManager => "package_manager",
            CandidateKind::Free => "free",
        }
        .to_string(),
    );
    entry
}

/// Kind/key labels that mark an entry as a fact read out of the repository
/// rather than something a person decided. Kept as data so existing entries
/// stay readable; excluded from automatic injection so a stale copy can never
/// contradict the working tree.
const DERIVED_LABELS: [&str; 1] = ["package_manager"];

/// Over-fetch this many extra hits before filtering, so dropping derived facts
/// does not silently shrink a full page of results.
const DERIVED_RECALL_SLACK: usize = 8;

/// What a memory IS, which decides how it reaches the model.
///
/// Stored as the existing free-form `kind` string so old JSON keeps loading;
/// this type is the parse boundary, not a schema change. `LegacyUnknown` is
/// deliberately distinct from `Note`: an entry written before the label
/// existed must not be promoted to a standing preference on a guess.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MemoryKind {
    /// Injected every turn while it is active.
    Preference,
    /// A decision worth keeping; reachable by query recall and the catalog.
    Decision,
    /// Everything else worth keeping; same reach as a decision.
    Note,
    /// Pre-dates the label. Query-only.
    LegacyUnknown,
    /// A fact read out of the repository. Kept, never injected.
    Derived,
}

impl MemoryKind {
    /// The wire/disk label.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Preference => "preference",
            Self::Decision => "decision",
            Self::Note => "note",
            Self::LegacyUnknown => "unknown",
            Self::Derived => "package_manager",
        }
    }

    /// Parse a stored entry's labels. `key` is consulted because older derived
    /// entries carry the marker there and not in `kind`.
    pub fn of(entry: &MemoryEntry) -> Self {
        if is_derived_fact(entry) {
            return Self::Derived;
        }
        match entry.kind.as_deref().map(str::trim) {
            Some("preference") => Self::Preference,
            Some("decision") => Self::Decision,
            Some("note") => Self::Note,
            _ if entry.tags.iter().any(|t| t.trim() == "preference") => Self::Preference,
            _ => Self::LegacyUnknown,
        }
    }
}

/// Whether this entry holds something that looks like a credential.
///
/// Legacy stores may already contain one: it is NOT deleted, but it is held
/// back from every automatic path and from the model's own memory tools.
pub fn is_sensitive(entry: &MemoryEntry) -> bool {
    looks_like_secret(&entry.title) || looks_like_secret(&entry.body)
}

/// Whether this entry declares itself a lasting preference.
///
/// A derived fact never counts, even when it carries a `preference` tag: the
/// repository is the authority for it. An entry with no label at all is not
/// promoted — older stores predate the label, and guessing would inject
/// arbitrary history into every turn.
pub fn is_standing_preference(entry: &MemoryEntry) -> bool {
    if is_derived_fact(entry) {
        return false;
    }
    entry.kind.as_deref().map(str::trim) == Some("preference")
        || entry.tags.iter().any(|t| t.trim() == "preference")
}

/// Whether this entry is a repository-derived fact (see [`DERIVED_LABELS`]).
/// Matches on `kind` or the structured `key`, because older entries carry the
/// label in one field or the other.
pub fn is_derived_fact(entry: &MemoryEntry) -> bool {
    let labelled = |value: &Option<String>| {
        value
            .as_deref()
            .is_some_and(|v| DERIVED_LABELS.contains(&v.trim()))
    };
    labelled(&entry.kind) || labelled(&entry.key)
}

pub(crate) fn slugify(title: &str) -> String {
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
        // A title with no ASCII at all — every CJK title — used to fall back to
        // a SECOND-precision timestamp. That made the id depend on when the
        // write happened: the same note saved twice across a second boundary
        // got two ids (so re-saving duplicated instead of being idempotent),
        // and two different notes inside one second got the SAME id (so one
        // silently replaced the other). Derive it from the title instead, so
        // the id is a function of the content and nothing else.
        format!("mem-{}", short_hash(&s_for_hash(title)))
    } else {
        s.chars().take(48).collect()
    }
}

/// What the fallback id hashes. Split out so the test can state the rule.
fn s_for_hash(title: &str) -> String {
    title.trim().to_string()
}

/// Short stable hex hash (FNV-1a 64), the same family `fingerprint_of` uses.
/// Not cryptographic — it only has to be stable and collision-shy for titles.
fn short_hash(raw: &str) -> String {
    let mut h: u64 = 0xcbf29ce484222325;
    for b in raw.bytes() {
        h ^= u64::from(b);
        h = h.wrapping_mul(0x100000001b3);
    }
    format!("{h:016x}")
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

pub(crate) fn now_rfc3339() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
}

pub(crate) fn write_atomically_pub(path: &Path, bytes: &[u8]) -> Result<(), MemoryError> {
    write_atomically(path, bytes)
}

#[cfg(test)]
mod tests {
    /// One fact must not end up as a direct write AND a candidate awaiting
    /// consent: the user would be asked to approve what is already saved.
    #[test]
    fn one_fact_yields_one_active_and_no_pending_residue() {
        let dir = tempfile::tempdir().unwrap();
        let store = MemoryStore::open(dir.path()).unwrap();
        let body = "提交前先运行 pnpm lint";

        // A soft signal proposed it first.
        let candidate = match store
            .propose_from_user_text(&format!("我通常希望{body}"))
            .unwrap()
            .unwrap()
        {
            ProposeOutcome::Pending(c) => c,
            other => panic!("{other:?}"),
        };
        assert_eq!(store.list_pending().unwrap().len(), 1);

        // Then the same content is activated (an approved agent proposal, or
        // the user writing it directly).
        store
            .activate(
                &candidate.title,
                &candidate.body,
                MemoryKind::Preference,
                vec![],
            )
            .unwrap();

        assert_eq!(store.list_active().unwrap().len(), 1, "one active");
        assert_eq!(
            store.list_pending().unwrap().len(),
            0,
            "no residue asking to approve what is stored"
        );
    }

    /// Crash residue heals on read: an activation that died before clearing
    /// the candidate must not leave a permanent duplicate.
    #[test]
    fn list_pending_heals_residue_left_by_a_crash() {
        let dir = tempfile::tempdir().unwrap();
        let store = MemoryStore::open(dir.path()).unwrap();
        let body = "提交前先运行 pnpm lint";
        let candidate = match store
            .propose_from_user_text(&format!("我通常希望{body}"))
            .unwrap()
            .unwrap()
        {
            ProposeOutcome::Pending(c) => c,
            other => panic!("{other:?}"),
        };
        // Active copy written directly, simulating a crash before cleanup.
        let mut entry = new_entry(&candidate.title, &candidate.body, vec![]);
        entry.kind = Some("preference".into());
        store.remember(entry).unwrap();
        assert_eq!(
            store.list_pending_raw().unwrap().len(),
            1,
            "residue present"
        );

        assert_eq!(
            store.list_pending().unwrap().len(),
            0,
            "the next read reconciles it away"
        );
    }

    /// The fallback id must come from the content, never from the clock: a
    /// second-precision timestamp made the same note duplicate across a second
    /// boundary and two different notes collide inside one.
    #[test]
    fn a_cjk_title_gets_a_content_derived_id() {
        let a = slugify("状态色约定");
        let b = slugify("状态色约定");
        let c = slugify("搜索框过滤范围");
        assert_eq!(a, b, "same title, same id, whatever the clock says");
        assert_ne!(a, c, "different titles must not collide");
        assert!(a.starts_with("mem-"), "{a}");
        assert!(!a.contains(':') && !a.contains('T'), "not a timestamp: {a}");
    }

    // ---- canonical activation ----

    /// Retrying the same direct write must not pile up copies.
    #[test]
    fn activating_identical_content_returns_the_existing_entry() {
        let dir = tempfile::tempdir().unwrap();
        let store = MemoryStore::open(dir.path()).unwrap();
        let a = store
            .activate("终端输出", "保持紧凑", MemoryKind::Preference, vec![])
            .unwrap();
        let b = store
            .activate("终端输出", "保持紧凑", MemoryKind::Preference, vec![])
            .unwrap();
        assert_eq!(a.id, b.id, "idempotent");
        assert_eq!(store.list_active().unwrap().len(), 1);
    }

    /// The same title with different content is a second memory, never a
    /// replacement — the bug that destroyed a user's earlier note.
    #[test]
    fn activating_a_repeated_title_never_overwrites() {
        let dir = tempfile::tempdir().unwrap();
        let store = MemoryStore::open(dir.path()).unwrap();
        let a = store
            .activate("部署说明", "第一版", MemoryKind::Note, vec![])
            .unwrap();
        let b = store
            .activate("部署说明", "第二版完全不同", MemoryKind::Note, vec![])
            .unwrap();
        assert_ne!(a.id, b.id);
        assert_eq!(store.list_active().unwrap().len(), 2);
        assert!(store.read_active(&a.id).unwrap().body.contains("第一版"));
    }

    /// Two CJK-only titles written in the same second must both land.
    #[test]
    fn cjk_titles_do_not_collide_on_a_second_boundary() {
        let dir = tempfile::tempdir().unwrap();
        let store = MemoryStore::open(dir.path()).unwrap();
        let a = store
            .activate("状态色约定", "用映射表", MemoryKind::Decision, vec![])
            .unwrap();
        let b = store
            .activate("搜索框范围", "只按 payer", MemoryKind::Decision, vec![])
            .unwrap();
        assert_ne!(a.id, b.id);
        assert_eq!(store.list_active().unwrap().len(), 2);
    }

    /// Secrets are refused at the domain boundary, so no UI, CLI or tool has
    /// to remember to check — and none can forget to.
    #[test]
    fn activation_refuses_a_secret_body() {
        let dir = tempfile::tempdir().unwrap();
        let store = MemoryStore::open(dir.path()).unwrap();
        let err = store.activate(
            "provider key",
            "OPENAI_API_KEY=sk-live-abcdefghijklmnopqrstuvwxyz0123456789",
            MemoryKind::Note,
            vec![],
        );
        assert!(err.is_err(), "a credential must not become memory");
        assert_eq!(store.list_active().unwrap().len(), 0);
    }

    /// Activating content a candidate already proposed clears that candidate:
    /// leaving it would ask the user to approve what is already stored.
    #[test]
    fn activation_clears_an_equivalent_pending_candidate() {
        let dir = tempfile::tempdir().unwrap();
        let store = MemoryStore::open(dir.path()).unwrap();
        let text = "我通常希望提交前先跑 lint";
        let candidate = match store.propose_from_user_text(text).unwrap().unwrap() {
            ProposeOutcome::Pending(c) => c,
            other => panic!("{other:?}"),
        };
        assert_eq!(store.list_pending().unwrap().len(), 1);
        store
            .activate(
                &candidate.title,
                &candidate.body,
                MemoryKind::Preference,
                vec![],
            )
            .unwrap();
        assert_eq!(store.list_active().unwrap().len(), 1);
        assert_eq!(
            store.list_pending().unwrap().len(),
            0,
            "no residue asking for consent to what is already active"
        );
    }

    // ---- catalog ----

    /// The catalog exists for discovery of things NOT already injected.
    #[test]
    fn the_catalog_excludes_preferences_derived_and_sensitive() {
        let dir = tempfile::tempdir().unwrap();
        let store = MemoryStore::open(dir.path()).unwrap();
        store
            .activate("紧凑输出", "保持紧凑", MemoryKind::Preference, vec![])
            .unwrap();
        store
            .activate("选了 SQLite", "三月的决定", MemoryKind::Decision, vec![])
            .unwrap();
        store
            .activate("一条笔记", "随手记的", MemoryKind::Note, vec![])
            .unwrap();
        let mut derived = new_entry("Package manager", "uses pnpm", vec![]);
        derived.kind = Some("package_manager".into());
        store.remember(derived).unwrap();
        // Legacy data predates the write boundary, so it is placed as a file
        // rather than written through the API that now refuses it.
        let mut secret = new_entry(
            "legacy token",
            "token=ghp_abcdefghijklmnopqrstuvwxyz0123",
            vec![],
        );
        secret.kind = Some("note".into());
        std::fs::write(
            dir.path()
                .join("active")
                .join(format!("{}.json", secret.id)),
            serde_json::to_string_pretty(&secret).unwrap(),
        )
        .unwrap();

        let catalog = store.catalog_lines(16).unwrap();
        assert!(catalog.contains("选了 SQLite"), "{catalog}");
        assert!(catalog.contains("一条笔记"), "{catalog}");
        assert!(
            !catalog.contains("紧凑输出"),
            "preference is already injected: {catalog}"
        );
        assert!(!catalog.contains("Package manager"), "{catalog}");
        assert!(!catalog.contains("legacy token"), "{catalog}");
        assert!(
            !catalog.contains("随手记的"),
            "no bodies in the catalog: {catalog}"
        );
    }

    /// Newest first, so a cap can never hide the decisions just made.
    #[test]
    fn the_catalog_is_newest_first_and_capped() {
        let dir = tempfile::tempdir().unwrap();
        let store = MemoryStore::open(dir.path()).unwrap();
        for i in 0..6 {
            let mut e = new_entry(&format!("决定 {i:02}"), "body", vec![]);
            e.kind = Some("decision".into());
            e.created_at = format!("2026-09-{:02}T00:00:00Z", i + 1);
            e.updated_at = e.created_at.clone();
            store.remember_deduplicated(e).unwrap();
        }
        let catalog = store.catalog_lines(3).unwrap();
        assert_eq!(catalog.lines().count(), 3, "capped: {catalog}");
        assert!(catalog.contains("决定 05"), "newest present: {catalog}");
        assert!(!catalog.contains("决定 00"), "oldest dropped: {catalog}");
    }

    // ---- sensitive withheld from every automatic path ----

    #[test]
    fn a_sensitive_legacy_entry_is_kept_but_withheld() {
        let dir = tempfile::tempdir().unwrap();
        let store = MemoryStore::open(dir.path()).unwrap();
        let mut secret = new_entry(
            "deploy token",
            "GITHUB_TOKEN=ghp_abcdefghijklmnopqrstuvwxyz0123",
            vec!["preference".into()],
        );
        secret.kind = Some("preference".into());
        // Legacy on-disk data: the write boundary would refuse it today.
        std::fs::write(
            dir.path()
                .join("active")
                .join(format!("{}.json", secret.id)),
            serde_json::to_string_pretty(&secret).unwrap(),
        )
        .unwrap();

        assert_eq!(store.list_active().unwrap().len(), 1, "not deleted");
        assert!(
            store.standing_preferences(8).unwrap().is_empty(),
            "never injected every turn"
        );
        assert!(
            store.recall("deploy token", 4).unwrap().is_empty(),
            "never query-recalled"
        );
        assert!(!store.catalog_lines(16).unwrap().contains("deploy token"));
    }

    /// Only entries that declare themselves preferences are injected
    /// unconditionally; everything else stays query-conditioned.
    #[test]
    fn standing_preferences_take_only_declared_preferences() {
        let dir = tempfile::tempdir().unwrap();
        let store = MemoryStore::open(dir.path()).unwrap();

        let mut by_kind = new_entry("Terse output", "keep the terminal compact", vec![]);
        by_kind.kind = Some("preference".into());
        store.remember_deduplicated(by_kind).unwrap();

        let by_tag = new_entry(
            "Review order",
            "correctness first",
            vec!["preference".into()],
        );
        store.remember_deduplicated(by_tag).unwrap();

        let mut a_fact = new_entry("Decision log", "we chose SQLite in March", vec![]);
        a_fact.kind = Some("fact".into());
        store.remember_deduplicated(a_fact).unwrap();

        // No kind at all: an older store predates the label, so it is NOT
        // promoted on a guess.
        store
            .remember_deduplicated(new_entry("Unlabelled", "something older", vec![]))
            .unwrap();

        let mut derived = new_entry("Package manager", "uses pnpm", vec!["preference".into()]);
        derived.kind = Some("package_manager".into());
        store.remember(derived).unwrap();

        let titles: Vec<String> = store
            .standing_preferences(8)
            .unwrap()
            .into_iter()
            .map(|e| e.title)
            .collect();
        assert_eq!(titles.len(), 2, "{titles:?}");
        assert!(titles.contains(&"Terse output".to_string()), "{titles:?}");
        assert!(titles.contains(&"Review order".to_string()), "{titles:?}");
    }

    /// The count is bounded and the order is stable, so the injected block is
    /// the same on every run.
    #[test]
    fn standing_preferences_are_bounded_and_deterministic() {
        let dir = tempfile::tempdir().unwrap();
        let store = MemoryStore::open(dir.path()).unwrap();
        for i in 0..12 {
            let mut e = new_entry(&format!("pref {i:02}"), &format!("body {i}"), vec![]);
            e.kind = Some("preference".into());
            store.remember_deduplicated(e).unwrap();
        }
        let first: Vec<String> = store
            .standing_preferences(5)
            .unwrap()
            .into_iter()
            .map(|e| e.id)
            .collect();
        assert_eq!(first.len(), 5, "bounded");
        let again: Vec<String> = store
            .standing_preferences(5)
            .unwrap()
            .into_iter()
            .map(|e| e.id)
            .collect();
        assert_eq!(first, again, "deterministic");
    }

    /// Repository-derived facts must stop being injected automatically, and
    /// must NOT be deleted: a user's active entry is theirs, and silently
    /// removing it would be the data loss this change exists to avoid.
    #[test]
    fn a_legacy_derived_entry_stays_visible_but_never_recalls() {
        let dir = tempfile::tempdir().unwrap();
        let store = MemoryStore::open(dir.path()).unwrap();
        let mut derived = new_entry("Package manager", "This project uses pnpm", vec![]);
        derived.kind = Some("package_manager".into());
        derived.key = Some("package_manager".into());
        store.remember(derived).unwrap();
        let mut real = new_entry("Deploy notes", "Release from pnpm-built artifacts", vec![]);
        real.kind = Some("preference".into());
        store.remember_deduplicated(real).unwrap();

        // Still on disk, still listable, still searchable by the user.
        assert_eq!(store.list_active().unwrap().len(), 2);
        let searched = store.search("pnpm", 5).unwrap();
        assert_eq!(searched.len(), 2, "user search still sees it: {searched:?}");

        // But automatic recall skips it.
        let recalled = store.recall("pnpm", 5).unwrap();
        assert_eq!(recalled.len(), 1, "recall dropped the derived fact");
        assert_eq!(recalled[0].0.title, "Deploy notes");
    }

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
}
