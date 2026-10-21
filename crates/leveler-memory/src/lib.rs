//! Project-scoped durable synthetic memory.
//!
//! - Writes are file-backed under a project memory root.
//! - Forget archives (does not hard-delete).
//! - Search is lexical BM25 over active entries only.
//! - INDEX is a short title list for cache-stable system injection; bodies stay
//!   out of the system prefix and are retrieved on demand.

#![forbid(unsafe_code)]

mod audit;
mod candidates;
mod lifecycle;
mod pipeline;
mod semantic;

pub use audit::{
    IdentityAudit, IdentityLocation, IdentityRecord, IdentityState, IdentitySummary,
};
pub use candidates::{
    CandidateKind, CandidateSource, MemoryCandidate, candidate_sensitive_fields, fingerprint_of,
    is_sensitive_candidate, looks_like_secret, package_manager_from_root,
    parse_direct_memory_command, parse_inferred_preference, redact_candidate, title_from_body,
};
pub use lifecycle::{
    AppliedOperation, CandidateOperation, CommitOutcome, MemoryAuthority, MemoryDecision,
    MemoryLifecycleOp, MemoryOperation, Provenance, agent_candidate, decide, parse_durable_fact,
    semantic_key_of,
};
pub use pipeline::{
    AdmitOutcome, AgentAdmitOutcome, ProposeOutcome, SuppressRecord, collect_turn_candidates,
};
pub use semantic::{
    CandidateDurability, CandidateRejection, CandidateScope, OperationHint, SemanticCandidate,
    SemanticError, SemanticMemoryType, ValidatedCandidates, canonical_subject, evidence_matches,
    parse_semantic_candidates, validate_semantic_candidate, validate_semantic_candidates,
};

use std::collections::HashMap;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// What a redacted historical record shows instead of the credential.
pub const REDACTED_TITLE: &str = "[sensitive memory]";
pub const REDACTED_BODY: &str = "[sensitive content removed]";

/// How far a memory's relevance reaches.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MemoryScope {
    /// Durable project knowledge; survives sessions and restarts.
    #[default]
    Project,
    /// Time-bound runtime state. Must carry an expiry and never becomes a
    /// lasting fact.
    Ephemeral,
}

/// Lifecycle state. `Active` is current truth; the other two are history.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MemoryStatus {
    #[default]
    Active,
    /// Replaced by a newer memory; kept for history, never recalled as current.
    Superseded,
    /// Reached its `expires_at`.
    Expired,
    /// Forgotten (archived) by the user.
    Archived,
}

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
    /// Doubles as the semantic identity for lifecycle conflict resolution.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub key: Option<String>,
    /// Free-form kind label (`preference`, `package_manager`, …).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
    /// Lifecycle state. Old files omit it and read as `Active`.
    #[serde(default)]
    pub status: MemoryStatus,
    /// Relevance scope. Old files omit it and read as `Project`.
    #[serde(default)]
    pub scope: MemoryScope,
    /// Where the fact came from and how much weight it carries.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provenance: Option<Provenance>,
    /// The id this entry replaced, when it is a correction.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub supersedes: Option<String>,
    /// The id that replaced this entry, when it is historical.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub superseded_by: Option<String>,
    /// When this entry stopped being current.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub superseded_at: Option<String>,
    /// When this entry stops being recallable. RFC 3339.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<String>,
}

impl MemoryEntry {
    /// The state after reconciling the legacy `archived_at` marker with the
    /// explicit `status` field: an old archived file (no status) reads as
    /// `Archived`, never as current truth.
    pub fn effective_status(&self) -> MemoryStatus {
        match self.status {
            MemoryStatus::Active if self.archived_at.is_some() => MemoryStatus::Archived,
            other => other,
        }
    }

    /// Whether this entry's window has closed. RFC 3339 UTC timestamps order
    /// lexicographically, so a string compare is exact here.
    pub fn is_expired_at(&self, now: &str) -> bool {
        self.expires_at.as_deref().is_some_and(|at| at <= now)
    }

    /// The authority this entry carries. An entry written before provenance
    /// existed is treated as the weakest class, so a new explicit statement may
    /// correct it.
    pub fn authority(&self) -> MemoryAuthority {
        self.provenance
            .as_ref()
            .map(|p| p.authority)
            .unwrap_or(MemoryAuthority::ModelInference)
    }
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
        let _lifecycle_lock = self.acquire_lifecycle_lock()?;
        self.remember_unlocked(entry)
    }

    pub(crate) fn remember_unlocked(&self, entry: MemoryEntry) -> Result<MemoryEntry, MemoryError> {
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
    pub fn remember_deduplicated(&self, entry: MemoryEntry) -> Result<MemoryEntry, MemoryError> {
        let _lifecycle_lock = self.acquire_lifecycle_lock()?;
        self.remember_deduplicated_unlocked(entry)
    }

    pub(crate) fn remember_deduplicated_unlocked(
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
            // History is durable data, not a pool of reusable identifiers.
            // Reusing an archived id would make the successful active write
            // below delete that history copy and can also create cycles in a
            // supersession chain. Explicit `remember` remains the deliberate
            // upsert/reactivation API; the deduplicating create path always
            // advances to a fresh id.
            if self.archive_path(&entry.id).exists() {
                suffix = suffix.checked_add(1).ok_or_else(|| {
                    MemoryError::Invalid("too many colliding memory ids".to_string())
                })?;
                continue;
            }
            let path = self.active_path(&entry.id);
            let json = serde_json::to_string_pretty(&entry)?;
            let mut temp = tempfile::Builder::new()
                .prefix(".memory-")
                .tempfile_in(self.root.join("active"))?;
            temp.write_all(json.as_bytes())?;
            temp.as_file().sync_all()?;

            match fs::hard_link(temp.path(), &path) {
                Ok(()) => {
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
        let _lifecycle_lock = self.acquire_lifecycle_lock()?;
        self.forget_unlocked(id)
    }

    pub(crate) fn forget_unlocked(&self, id: &str) -> Result<MemoryEntry, MemoryError> {
        let active = self.active_path(id);
        let archive = self.archive_path(id);
        if active.exists() {
            let mut entry: MemoryEntry = serde_json::from_str(&fs::read_to_string(&active)?)?;
            entry.archived_at = Some(now_rfc3339());
            entry.status = MemoryStatus::Archived;
            // Forget is a retention boundary. The record stays for audit, but
            // a credential must not be copied into the archive in plaintext.
            if is_sensitive(&entry) {
                redact_entry(&mut entry);
            }
            write_atomically(&archive, serde_json::to_string_pretty(&entry)?.as_bytes())?;
            fs::remove_file(active)?;
            return Ok(entry);
        }
        if archive.exists() {
            let mut entry: MemoryEntry = serde_json::from_str(&fs::read_to_string(&archive)?)?;
            if is_sensitive(&entry) {
                // Idempotent forget is also the per-id cleanup path for an
                // archive written before this boundary existed.
                redact_entry(&mut entry);
                write_atomically(&archive, serde_json::to_string_pretty(&entry)?.as_bytes())?;
            }
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
        // The legacy index is still a model-facing surface even though the
        // catalog replaced it. A historical credential must not appear here
        // either.
        let entries: Vec<MemoryEntry> = self
            .effective_active()?
            .into_iter()
            .filter(|entry| !is_sensitive(entry))
            .collect();
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
    /// Guarantees: a non-assignment body never overwrites — identical
    /// title+body returns the entry that already exists (so a retry is
    /// idempotent) and the same title with different content gets its own id.
    /// A body that states a durable assignment ("以后…固定为 X") instead runs
    /// the shared lifecycle: it updates the current value for that semantic
    /// identity and preserves the old value as superseded history. Secrets are
    /// refused here rather than in each UI, and an equivalent pending
    /// candidate is cleaned up once the active copy is durable — a failure to
    /// clean up leaves the active entry in place rather than rolling it back.
    pub fn activate(
        &self,
        title: &str,
        body: &str,
        kind: MemoryKind,
        tags: Vec<String>,
    ) -> Result<MemoryEntry, MemoryError> {
        let _lifecycle_lock = self.acquire_lifecycle_lock()?;
        // A user write that states an assignment ("以后默认模型固定为 Pro")
        // carries the same semantic identity as every other path. It therefore
        // runs the same lifecycle decision: a later value replaces the earlier
        // one and the old value becomes history, instead of accumulating a
        // second current truth. A non-assignment body stays a plain create.
        if let Some(mut candidate) = crate::parse_durable_fact(body) {
            candidate.title = title.trim().to_string();
            candidate.body = body.trim().to_string();
            candidate.kind = kind.candidate_kind();
            candidate
                .tags
                .retain(|tag| !tag.trim_start().starts_with("kind:"));
            candidate
                .tags
                .push(format!("kind:{}", kind.as_str()));
            candidate.authority = MemoryAuthority::ExplicitUser;
            candidate.source = CandidateSource::UserExplicit;
            if candidate.evidence.is_none() {
                candidate.evidence = Some(body.trim().to_string());
            }
            let outcome = self.apply_candidate_locked(&candidate, &now_rfc3339())?;
            return match outcome.operation {
                AppliedOperation::Created
                | AppliedOperation::Superseded
                | AppliedOperation::Merged => {
                    let saved = outcome.entry.ok_or_else(|| {
                        MemoryError::Invalid(
                            "memory write produced no current entry".to_string(),
                        )
                    })?;
                    // Best effort, after the active copy is durable, exactly
                    // like the non-assignment path below.
                    let _ = self.clear_equivalent_pending(&saved);
                    Ok(saved)
                }
                AppliedOperation::Skipped => {
                    let key = candidate
                        .semantic_key
                        .as_deref()
                        .or(candidate.key.as_deref());
                    self.effective_active()?
                        .into_iter()
                        .find(|entry| {
                            key.is_some_and(|key| {
                                crate::lifecycle::durable_entry_key(entry).as_deref() == Some(key)
                            })
                        })
                        .ok_or_else(|| {
                            MemoryError::Invalid(
                                outcome
                                    .skipped_reason
                                    .unwrap_or_else(|| "memory was not stored".to_string()),
                            )
                        })
                }
                AppliedOperation::NeedsApproval => Err(MemoryError::Invalid(
                    outcome
                        .skipped_reason
                        .unwrap_or_else(|| "memory needs a person".to_string()),
                )),
            };
        }
        let mut entry = new_entry(title, body, tags);
        entry.kind = Some(kind.as_str().to_string());
        // Every caller of `activate` is a person's own command or a proposal
        // they approved, so the entry records explicit-user authority. Without
        // it an entry written before this field existed and one written now
        // would look alike, and a lower-authority observation could silently
        // replace a fact the user chose.
        entry.provenance = Some(Provenance {
            authority: MemoryAuthority::ExplicitUser,
            source: "user_direct".to_string(),
            evidence: None,
        });
        // `remember_deduplicated` already owns the hard parts: a hard-link
        // reservation so concurrent writers cannot claim one id, an identical
        // title+body returning what is there, and a `-N` suffix otherwise.
        // `validate_entry` inside it is where a credential is refused.
        let saved = self.remember_deduplicated_unlocked(entry)?;
        // Best effort, deliberately after the active copy is durable: a
        // candidate left behind would ask the user to approve what is already
        // stored, but failing to remove it is not a reason to lose the write.
        // The domain deliberately takes no logger; a failure here is healed by
        // `list_pending`, which drops covered candidates on the next read.
        let _ = self.clear_equivalent_pending(&saved);
        Ok(saved)
    }

    /// Create an active memory from the model's own `remember` proposal.
    ///
    /// Human approval authorizes the WRITE; it does not turn the model's
    /// finding into something the user said. The entry therefore carries
    /// model-inference authority and an `agent_remember_tool` source, and it
    /// goes through the non-overwriting create path so it can never replace a
    /// fact the user wrote directly.
    pub fn activate_agent(
        &self,
        title: &str,
        body: &str,
        kind: MemoryKind,
        tags: Vec<String>,
    ) -> Result<MemoryEntry, MemoryError> {
        let _lifecycle_lock = self.acquire_lifecycle_lock()?;
        let mut entry = new_entry(title, body, tags);
        entry.kind = Some(kind.as_str().to_string());
        entry.provenance = Some(Provenance {
            authority: MemoryAuthority::ModelInference,
            source: "agent_remember_tool".to_string(),
            evidence: None,
        });
        let saved = self.remember_deduplicated_unlocked(entry)?;
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
            .effective_active()?
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
            .effective_active()?
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
        let entries = self.effective_active()?;
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
    if is_sensitive(entry) {
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
        status: MemoryStatus::Active,
        scope: MemoryScope::Project,
        provenance: None,
        supersedes: None,
        superseded_by: None,
        superseded_at: None,
        expires_at: None,
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
    // A caller that means `decision`/`note` records it in the tags; the
    // candidate category alone cannot tell those apart from a generic fact.
    let declared_kind = candidate
        .tags
        .iter()
        .find_map(|tag| tag.trim().strip_prefix("kind:"))
        .map(str::trim)
        .filter(|kind| matches!(*kind, "preference" | "decision" | "note"));
    entry.kind = Some(
        declared_kind
            .map(str::to_string)
            .unwrap_or_else(|| {
                match candidate.kind {
                    CandidateKind::Preference => "preference",
                    CandidateKind::PackageManager => "package_manager",
                    CandidateKind::Free => "free",
                }
                .to_string()
            }),
    );
    entry.provenance = Some(Provenance {
        authority: candidate.authority,
        source: match candidate.source {
            crate::CandidateSource::UserExplicit => "explicit_user",
            crate::CandidateSource::SystemPropose => "system_propose",
            crate::CandidateSource::SystemInferred => "system_inferred",
            crate::CandidateSource::AgentProposed => "agent_remember_tool",
            crate::CandidateSource::UserRestore => "user_restore",
            crate::CandidateSource::UserAccepted => "user_accepted",
        }
        .to_string(),
        evidence: None,
    });
    entry.expires_at = candidate.expires_at.clone();
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

    /// The candidate category that carries this kind through the lifecycle.
    /// `preference` stays distinct because it is injected every turn; the
    /// decision/note distinction is preserved by a `kind:` tag instead.
    pub fn candidate_kind(self) -> CandidateKind {
        match self {
            Self::Preference => CandidateKind::Preference,
            Self::Derived => CandidateKind::PackageManager,
            Self::Decision | Self::Note | Self::LegacyUnknown => CandidateKind::Free,
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
    !sensitive_fields(entry).is_empty()
}

/// Which durable fields of an entry carry credential-shaped material.
fn sensitive_fields(entry: &MemoryEntry) -> Vec<&'static str> {
    let mut fields = Vec::new();
    if looks_like_secret(&entry.title) {
        fields.push("title");
    }
    if looks_like_secret(&entry.body) {
        fields.push("body");
    }
    if entry.tags.iter().any(|tag| looks_like_secret(tag)) {
        fields.push("tags");
    }
    // A semantic key is durable content too. A credential there would reach
    // the audit report, the pending filename and the entry id.
    if entry.key.as_deref().is_some_and(looks_like_secret) {
        fields.push("key");
    }
    if entry
        .provenance
        .as_ref()
        .and_then(|provenance| provenance.evidence.as_deref())
        .is_some_and(looks_like_secret)
    {
        fields.push("evidence");
    }
    fields
}

/// Redact the credential-shaped fields of an entry in place. Safe fields are
/// preserved, so a redacted record still says what kind of memory it was.
pub(crate) fn redact_entry_for_archive(entry: &mut MemoryEntry) {
    redact_entry(entry);
}

fn redact_entry(entry: &mut MemoryEntry) {
    if looks_like_secret(&entry.title) {
        entry.title = REDACTED_TITLE.to_string();
    }
    if looks_like_secret(&entry.body) {
        entry.body = REDACTED_BODY.to_string();
    }
    entry.tags.retain(|tag| !looks_like_secret(tag));
    if entry.key.as_deref().is_some_and(looks_like_secret) {
        // The key is an identity, not a value. A credential-shaped key is
        // removed rather than replaced with a placeholder that could collide
        // with another redacted record.
        entry.key = None;
    }
    if let Some(provenance) = entry.provenance.as_mut()
        && provenance
            .evidence
            .as_deref()
            .is_some_and(looks_like_secret)
    {
        provenance.evidence = None;
    }
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
pub(crate) fn short_hash(raw: &str) -> String {
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

    /// Old stores can contain credentials that predate (or slipped past) the
    /// write boundary. They remain user-owned files, but no automatic model path
    /// may expose them.
    #[test]
    fn legacy_api_keys_are_withheld_from_recall_and_catalog() {
        let dir = tempfile::tempdir().unwrap();
        let store = MemoryStore::open(dir.path()).unwrap();
        for (id, title, body) in [
            (
                "legacy-api-key",
                "API key",
                "API key is demo-provider-value-1234567890",
            ),
            (
                "legacy-deploy-key",
                "部署密钥",
                "部署密钥是 demo-provider-value-1234567890",
            ),
        ] {
            let mut entry = new_entry(title, body, vec!["preference".into()]);
            entry.id = id.to_string();
            entry.kind = Some("preference".into());
            std::fs::write(
                dir.path().join("active").join(format!("{id}.json")),
                serde_json::to_string_pretty(&entry).unwrap(),
            )
            .unwrap();
        }

        assert_eq!(store.list_active().unwrap().len(), 2, "files are preserved");
        assert!(
            store.list_active().unwrap().iter().all(is_sensitive),
            "every legacy credential must be classified as sensitive"
        );
        assert!(store.standing_preferences(8).unwrap().is_empty());
        assert!(store.recall("API key", 4).unwrap().is_empty());
        assert!(store.recall("部署密钥", 4).unwrap().is_empty());
        assert!(store.catalog_lines(16).unwrap().is_empty());
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

    /// Crash residue can leave two active files for one identity (the replacement
    /// is written before the old one is archived). Every read that presents
    /// "current" truth must collapse them; the legacy index is no exception.
    #[test]
    fn the_legacy_index_never_lists_a_stale_duplicate_of_a_current_identity() {
        let dir = tempdir().unwrap();
        let store = MemoryStore::open(dir.path()).unwrap();

        let mut old = new_entry("默认模型", "默认模型：Pro", vec![]);
        old.id = "legacy-pro".into();
        old.key = Some("k-defaultmodel".into());
        old.provenance = Some(Provenance {
            authority: MemoryAuthority::ExplicitUser,
            source: "explicit_user_turn".into(),
            evidence: None,
        });
        old.created_at = "2026-01-01T00:00:00Z".into();
        old.updated_at = old.created_at.clone();
        store.remember(old.clone()).unwrap();

        let mut new = new_entry("默认模型", "默认模型：Flash", vec![]);
        new.id = "legacy-flash".into();
        new.key = Some("k-defaultmodel".into());
        new.provenance = Some(Provenance {
            authority: MemoryAuthority::ExplicitUser,
            source: "explicit_user_turn".into(),
            evidence: None,
        });
        new.created_at = "2026-01-02T00:00:00Z".into();
        new.updated_at = new.created_at.clone();
        new.supersedes = Some(old.id.clone());
        store.remember(new.clone()).unwrap();

        let index = store.index_lines(10).unwrap();
        assert!(index.contains(&new.id), "{index}");
        assert!(
            !index.contains(&old.id),
            "a stale active copy was presented as current: {index}"
        );
    }

    /// The supersedes link is authoritative even when both files carry the same
    /// second-precision timestamp: after a crash, the terminal entry is the
    /// current truth, not whichever title sorts first.
    #[test]
    fn crash_residue_selects_the_terminal_entry_not_the_first_title() {
        let dir = tempdir().unwrap();
        let store = MemoryStore::open(dir.path()).unwrap();

        let mut old = new_entry("A old value", "默认模型：Pro", vec![]);
        old.id = "old-pro".into();
        old.key = Some("k-defaultmodel".into());
        old.provenance = Some(Provenance {
            authority: MemoryAuthority::ExplicitUser,
            source: "explicit_user_turn".into(),
            evidence: None,
        });
        old.created_at = "2026-01-01T00:00:00Z".into();
        old.updated_at = old.created_at.clone();
        store.remember(old).unwrap();

        let mut new = new_entry("B new value", "默认模型：Flash", vec![]);
        new.id = "new-flash".into();
        new.key = Some("k-defaultmodel".into());
        new.provenance = Some(Provenance {
            authority: MemoryAuthority::ExplicitUser,
            source: "explicit_user_turn".into(),
            evidence: None,
        });
        new.created_at = "2026-01-01T00:00:00Z".into();
        new.updated_at = new.created_at.clone();
        new.supersedes = Some("old-pro".into());
        store.remember(new).unwrap();

        let active = store.effective_active().unwrap();
        assert_eq!(active.len(), 1);
        assert_eq!(active[0].id, "new-flash");
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
    /// Audit historical records that carry credential-shaped material.
    pub fn audit_sensitive(&self) -> Result<Vec<SensitiveFinding>, MemoryError> {
        let mut findings = Vec::new();
        for entry in self.list_active()? {
            let fields = sensitive_fields(&entry);
            if !fields.is_empty() {
                findings.push(SensitiveFinding {
                    id: entry.id,
                    location: SensitiveLocation::Active,
                    fields,
                });
            }
        }
        for entry in self.list_archived()? {
            let fields = sensitive_fields(&entry);
            if !fields.is_empty() {
                findings.push(SensitiveFinding {
                    id: entry.id,
                    location: SensitiveLocation::Archived,
                    fields,
                });
            }
        }
        for candidate in self.list_pending_raw()? {
            let fields = candidate_sensitive_fields(&candidate);
            if !fields.is_empty() {
                findings.push(SensitiveFinding {
                    id: candidate.id,
                    location: SensitiveLocation::Pending,
                    fields,
                });
            }
        }
        Ok(findings)
    }

    /// Redact credential-shaped fields in historical records. A dry run reads
    /// only; `apply` rewrites whichever stores contain a finding.
    pub fn scrub_sensitive(
        &self,
        id: Option<&str>,
        apply: bool,
    ) -> Result<ScrubReport, MemoryError> {
        let _lifecycle_lock = if apply {
            Some(self.acquire_lifecycle_lock()?)
        } else {
            None
        };
        let mut report = ScrubReport::default();
        let matches = |candidate_id: &str| id.is_none_or(|wanted| wanted == candidate_id);

        for mut entry in self.list_active()? {
            if !matches(&entry.id) {
                continue;
            }
            let fields = sensitive_fields(&entry);
            if fields.is_empty() {
                continue;
            }
            report.findings += 1;
            if apply {
                redact_entry(&mut entry);
                write_atomically(
                    &self.active_path(&entry.id),
                    serde_json::to_string_pretty(&entry)?.as_bytes(),
                )?;
                report.redacted += 1;
            }
        }
        for mut entry in self.list_archived()? {
            if !matches(&entry.id) {
                continue;
            }
            let fields = sensitive_fields(&entry);
            if fields.is_empty() {
                continue;
            }
            report.findings += 1;
            if apply {
                redact_entry(&mut entry);
                write_atomically(
                    &self.archive_path(&entry.id),
                    serde_json::to_string_pretty(&entry)?.as_bytes(),
                )?;
                report.redacted += 1;
            }
        }
        for mut candidate in self.list_pending_raw()? {
            if !matches(&candidate.id) {
                continue;
            }
            let fields = candidate_sensitive_fields(&candidate);
            if fields.is_empty() {
                continue;
            }
            report.findings += 1;
            if apply {
                redact_candidate(&mut candidate);
                write_atomically(
                    &self.pending_path(&candidate.id),
                    serde_json::to_string_pretty(&candidate)?.as_bytes(),
                )?;
                report.redacted += 1;
            }
        }
        Ok(report)
    }

    /// Dense local-vector search (cosine over [`local_embed`]).
    pub fn vector_search(
        &self,
        query: &str,
        limit: usize,
    ) -> Result<Vec<(MemoryEntry, f64)>, MemoryError> {
        let q = local_embed(query);
        let entries = self.effective_active()?;
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

/// Where a historical sensitive memory lives.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SensitiveLocation {
    Active,
    Archived,
    Pending,
}

/// One historical credential-bearing record. Never carries the content.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SensitiveFinding {
    pub id: String,
    pub location: SensitiveLocation,
    pub fields: Vec<&'static str>,
}

/// Result of a scrub pass. `findings` counts what the audit saw; `redacted`
/// counts records actually rewritten (always 0 for a dry run).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ScrubReport {
    pub findings: usize,
    pub redacted: usize,
}

#[cfg(test)]
mod safety_tests {
    use super::*;
    use tempfile::tempdir;

    fn write_legacy_active(root: &std::path::Path, entry: &MemoryEntry) {
        std::fs::write(
            root.join("active").join(format!("{}.json", entry.id)),
            serde_json::to_string_pretty(entry).unwrap(),
        )
        .unwrap();
    }

    fn legacy_secret(title: &str, body: &str) -> MemoryEntry {
        let mut entry = new_entry(title, body, vec![]);
        entry.kind = Some("note".into());
        entry
    }

    /// Every field that reaches durability is a credential boundary, not only
    /// title and body. Tags and provenance evidence used to bypass the gate.
    #[test]
    fn activation_refuses_a_secret_in_a_tag() {
        let dir = tempdir().unwrap();
        let store = MemoryStore::open(dir.path()).unwrap();
        let error = store
            .activate(
                "Deploy notes",
                "Use the release branch.",
                MemoryKind::Note,
                vec!["token=ghp_abcdefghijklmnopqrstuvwxyz0123".into()],
            )
            .unwrap_err();
        assert!(matches!(error, MemoryError::Invalid(_)), "{error}");
        assert!(store.list_active().unwrap().is_empty());
    }

    #[test]
    fn activation_refuses_a_secret_in_a_key() {
        let dir = tempdir().unwrap();
        let store = MemoryStore::open(dir.path()).unwrap();
        let mut entry = new_entry("Deploy notes", "Use the release branch.", vec![]);
        entry.key = Some("ghp_abcdefghijklmnopqrstuvwxyz0123".into());
        let error = store.remember(entry).unwrap_err();
        assert!(matches!(error, MemoryError::Invalid(_)), "{error}");
        assert!(store.list_active().unwrap().is_empty());
    }

    #[test]
    fn a_secret_key_marks_a_legacy_entry_sensitive_and_scrub_clears_it() {
        let dir = tempdir().unwrap();
        let store = MemoryStore::open(dir.path()).unwrap();
        let secret = "ghp_abcdefghijklmnopqrstuvwxyz0123";
        let mut entry = legacy_secret("legacy-key", "safe body");
        entry.key = Some(secret.to_string());
        write_legacy_active(dir.path(), &entry);

        assert!(is_sensitive(&entry));
        let findings = store.audit_sensitive().unwrap();
        assert_eq!(findings.len(), 1);
        assert!(findings[0].fields.contains(&"key"));

        let report = store.scrub_sensitive(None, true).unwrap();
        assert_eq!(report.redacted, 1);
        let raw = std::fs::read_to_string(store.active_path(&entry.id)).unwrap();
        assert!(!raw.contains(secret), "{raw}");
        let scrubbed = store.read_active(&entry.id).unwrap();
        assert!(scrubbed.key.is_none());
    }

    #[test]
    fn activation_refuses_a_secret_in_provenance_evidence() {
        let dir = tempdir().unwrap();
        let store = MemoryStore::open(dir.path()).unwrap();
        let mut entry = new_entry("Deploy notes", "Use the release branch.", vec![]);
        entry.provenance = Some(Provenance {
            authority: MemoryAuthority::ExplicitUser,
            source: "user_direct".into(),
            evidence: Some("password=hunter2hunter2".into()),
        });
        let error = store.remember(entry).unwrap_err();
        assert!(matches!(error, MemoryError::Invalid(_)), "{error}");
        assert!(store.list_active().unwrap().is_empty());
    }

    #[test]
    fn a_secret_in_a_tag_or_evidence_marks_a_legacy_entry_sensitive() {
        let dir = tempdir().unwrap();
        let store = MemoryStore::open(dir.path()).unwrap();

        let mut tagged = legacy_secret("tagged", "safe body");
        tagged.tags = vec!["sk-live-abcdefghijklmnopqrstuvwxyz012345".into()];
        let mut evidenced = legacy_secret("evidenced", "safe body");
        evidenced.provenance = Some(Provenance {
            authority: MemoryAuthority::ExplicitUser,
            source: "explicit_user_turn".into(),
            evidence: Some("ghp_abcdefghijklmnopqrstuvwxyz0123".into()),
        });
        write_legacy_active(dir.path(), &tagged);
        write_legacy_active(dir.path(), &evidenced);

        assert!(is_sensitive(&tagged));
        assert!(is_sensitive(&evidenced));
        assert!(store.standing_preferences(8).unwrap().is_empty());
        assert!(store.catalog_lines(16).unwrap().is_empty());
        assert!(store.index_lines(16).unwrap().is_empty());
    }

    /// Forget is a retention boundary: the archive is still on disk for audit,
    /// but a credential must not be copied into it in plaintext.
    #[test]
    fn forgetting_a_sensitive_entry_does_not_archive_the_plaintext() {
        let dir = tempdir().unwrap();
        let store = MemoryStore::open(dir.path()).unwrap();
        let secret = "ghp_abcdefghijklmnopqrstuvwxyz0123";
        let entry = legacy_secret("Deploy token", &format!("token={secret}"));
        write_legacy_active(dir.path(), &entry);

        let archived = store.forget(&entry.id).unwrap();
        let raw = std::fs::read_to_string(store.archive_path(&entry.id)).unwrap();
        assert!(!raw.contains(secret), "archive kept the credential: {raw}");
        assert!(!archived.body.contains(secret));
        assert_eq!(archived.status, MemoryStatus::Archived);
        assert!(store.archive_path(&entry.id).exists());
    }

    #[test]
    fn expiry_archives_a_legacy_secret_without_its_plaintext() {
        let dir = tempdir().unwrap();
        let store = MemoryStore::open(dir.path()).unwrap();
        let secret = "ghp_abcdefghijklmnopqrstuvwxyz0123";
        let mut entry = legacy_secret("Expiring token", &format!("token={secret}"));
        entry.expires_at = Some("2020-01-01T00:00:00Z".into());
        write_legacy_active(dir.path(), &entry);

        let expired = store.expire_due().unwrap();
        assert_eq!(expired.len(), 1);
        let raw = std::fs::read_to_string(store.archive_path(&entry.id)).unwrap();
        assert!(!raw.contains(secret), "expiry kept the credential: {raw}");
    }

    #[test]
    fn audit_lists_locations_and_scrub_redacts_active_archive_and_pending() {
        let dir = tempdir().unwrap();
        let store = MemoryStore::open(dir.path()).unwrap();
        let secret = "ghp_abcdefghijklmnopqrstuvwxyz0123";

        let active = legacy_secret("Active token", &format!("token={secret}"));
        write_legacy_active(dir.path(), &active);
        let archived = legacy_secret("Archived token", &format!("token={secret}"));
        std::fs::write(
            dir.path()
                .join("archive")
                .join(format!("{}.json", archived.id)),
            serde_json::to_string_pretty(&archived).unwrap(),
        )
        .unwrap();
        let pending = MemoryCandidate::new(
            "Pending token",
            "safe body",
            CandidateKind::Free,
            None,
            CandidateSource::AgentProposed,
            vec![],
        )
        .unwrap();
        let mut pending = pending;
        pending.body = format!("token={secret}");
        std::fs::write(
            dir.path()
                .join("pending")
                .join(format!("{}.json", pending.id)),
            serde_json::to_string_pretty(&pending).unwrap(),
        )
        .unwrap();

        let findings = store.audit_sensitive().unwrap();
        assert_eq!(findings.len(), 3, "{findings:?}");
        assert!(
            findings
                .iter()
                .any(|f| f.location == SensitiveLocation::Active)
        );
        assert!(
            findings
                .iter()
                .any(|f| f.location == SensitiveLocation::Archived)
        );
        assert!(
            findings
                .iter()
                .any(|f| f.location == SensitiveLocation::Pending)
        );

        let dry = store.scrub_sensitive(None, false).unwrap();
        assert_eq!(dry.redacted, 0);
        assert_eq!(dry.findings, 3);
        let raw = std::fs::read_to_string(store.active_path(&active.id)).unwrap();
        assert!(raw.contains(secret), "dry run must not touch user data");

        let applied = store.scrub_sensitive(None, true).unwrap();
        assert_eq!(applied.redacted, 3);
        for path in [
            store.active_path(&active.id),
            store.archive_path(&archived.id),
            store.pending_path(&pending.id),
        ] {
            let raw = std::fs::read_to_string(&path).unwrap();
            assert!(!raw.contains(secret), "scrub left a credential in {path:?}");
        }
        assert!(store.audit_sensitive().unwrap().is_empty());
    }
}
