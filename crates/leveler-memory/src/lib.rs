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
mod experience;
mod lanes;
mod lifecycle;
mod pipeline;
mod semantic;
pub mod service;
pub mod v2;

pub use audit::{IdentityAudit, IdentityLocation, IdentityRecord, IdentityState, IdentitySummary};
pub use candidates::{
    CandidateKind, CandidateSource, MemoryCandidate, candidate_sensitive_fields, fingerprint_of,
    is_high_risk_memory, is_sensitive_candidate, looks_like_secret, package_manager_from_root,
    parse_direct_memory_command, redact_candidate, title_from_body,
};
pub use experience::{
    EXPERIENCE_KIND, ExperienceCategory, ExperienceEvidence, ExperienceRecord, ExperienceStanding,
    ExperienceVerification, validate_experience_record,
};
pub use lanes::MemoryLanes;
pub use lifecycle::{
    AppliedOperation, CandidateOperation, CommitOutcome, MemoryAuthority, MemoryDecision,
    MemoryLifecycleOp, MemoryOperation, Provenance, agent_candidate, decide, semantic_key_of,
};
pub use pipeline::{
    AdmitOutcome, AgentAdmitOutcome, PendingDisposition, ProposeOutcome, SuppressRecord,
    candidate_eligible_for_activation,
};
pub use semantic::{
    CandidateDurability, CandidateRejection, CandidateScope, OperationHint, SemanticCandidate,
    SemanticError, SemanticMemoryType, StatementKind, ValidatedCandidates, evidence_matches,
    normalize_subject, parse_semantic_candidates, validate_semantic_candidate,
    validate_semantic_candidates,
};
pub use service::{
    ExperienceView, IdentityContextEntry, MARKDOWN_SEARCH_UNAVAILABLE, MemoryRoots, MemoryService,
    MemoryView, RememberRequest, ReviewRequest, ServiceError, ServiceScope, UpdateRequest,
    WriteOutcome,
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
    /// The value the fact assigns, when the statement is an assignment. Stored
    /// structurally so "same subject, same value" is an exact field compare,
    /// never a comparison of two sentences.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub value: Option<String>,
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
    /// The evidence-bound half of an agent experience. `None` on every other
    /// kind, including all entries written before this field existed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub experience: Option<ExperienceRecord>,
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
            .filter(|entry| {
                // Experience is relevance-retrieved, never part of a standing
                // index paid for on every request.
                !is_sensitive(entry) && MemoryKind::of(entry) != MemoryKind::Experience
            })
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
    /// Store a memory the person wrote directly.
    ///
    /// The person's own command is the authority, but no meaning is parsed
    /// out of the prose: identity, scope and relations are model-supplied
    /// structured fields on the proposal paths. A direct write is therefore a
    /// plain create (deduplicated by title+body) and never silently replaces
    /// another entry. Secrets are refused here rather than in each UI.
    pub fn activate(
        &self,
        title: &str,
        body: &str,
        kind: MemoryKind,
        tags: Vec<String>,
    ) -> Result<MemoryEntry, MemoryError> {
        // An experience without its structured evidence would claim a
        // verification that never happened. Only the evidence-bound admission
        // path may create one.
        if kind == MemoryKind::Experience {
            return Err(MemoryError::Invalid(
                "experience requires runtime evidence; it cannot be written directly".to_string(),
            ));
        }
        let _lifecycle_lock = self.acquire_lifecycle_lock()?;
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
            // The user's own statement is the evidence this entry may be
            // reconciled against later. Storing it does not infer a subject;
            // it only records what the person actually wrote.
            evidence: Some(body.trim().to_string()),
        });
        // `remember_deduplicated` already owns the hard parts: a hard-link
        // reservation so concurrent writers cannot claim one id, an identical
        // title+body returning what is there, and a `-N` suffix otherwise.
        // `validate_entry` inside it is where a credential is refused.
        let saved = self.remember_deduplicated_unlocked(entry)?;
        // After the active copy is durable: a candidate left behind would ask
        // the user to approve what is already stored. The settlement is
        // recorded before the pending file is removed.
        self.clear_equivalent_pending(&saved)?;
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
        if kind == MemoryKind::Experience {
            return Err(MemoryError::Invalid(
                "experience requires runtime evidence; it cannot be written directly".to_string(),
            ));
        }
        let _lifecycle_lock = self.acquire_lifecycle_lock()?;
        let mut entry = new_entry(title, body, tags);
        entry.kind = Some(kind.as_str().to_string());
        entry.provenance = Some(Provenance {
            authority: MemoryAuthority::ModelInference,
            source: "agent_remember_tool".to_string(),
            evidence: None,
        });
        let saved = self.remember_deduplicated_unlocked(entry)?;
        self.clear_equivalent_pending(&saved)?;
        Ok(saved)
    }

    /// Drop pending candidates whose content this active entry now covers.
    ///
    /// Also run from [`Self::list_pending`], so a crash between the two steps
    /// heals on the next read instead of leaving a permanent duplicate. Each
    /// removal is recorded before the file is deleted; pending is not history.
    fn clear_equivalent_pending(&self, active: &MemoryEntry) -> Result<(), MemoryError> {
        for candidate in self.list_pending_raw()? {
            if candidate.body.trim() != active.body.trim() {
                continue;
            }
            self.record_pending_disposition(&crate::PendingDisposition {
                candidate_id: candidate.id.clone(),
                key: candidate.semantic_key.clone().or(candidate.key.clone()),
                outcome: "covered_by_direct".to_string(),
                committed_id: Some(active.id.clone()),
                decided_at: now_rfc3339(),
            })?;
            match fs::remove_file(self.pending_path(&candidate.id)) {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(MemoryError::Io(error)),
            }
        }
        Ok(())
    }

    /// Titles the model may ask about but which are NOT injected every turn.
    ///
    /// Replaces the old "all active titles" index. Its one job is discovery:
    /// when query recall misses on wording, a decision or note can still be
    /// found by title and read with the `memory` tool. Preferences are excluded
    /// because they are already injected in full; experience entries because
    /// they are retrieved only when relevant, never carried in every request;
    /// derived and sensitive entries because they must never reach the model
    /// this way.
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
        self.recall_for_lanes(query, limit, MemoryLanes::ALL)
    }

    /// Retrieval restricted to the lanes a turn may read.
    ///
    /// The search runs over every active entry before the lane filter, so a
    /// high-scoring entry in a disabled lane cannot consume the caller's
    /// limit and hide an eligible one. This is the same reason sensitive and
    /// repository-derived entries are filtered after scoring.
    pub fn recall_for_lanes(
        &self,
        query: &str,
        limit: usize,
        lanes: MemoryLanes,
    ) -> Result<Vec<(MemoryEntry, f64)>, MemoryError> {
        if !lanes.any() {
            return Ok(Vec::new());
        }
        let mut hits = self.search(query, usize::MAX)?;
        hits.retain(|(entry, _)| {
            !is_derived_fact(entry) && !is_sensitive(entry) && lanes.allows_entry(entry)
        });
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
        let doc_texts: Vec<String> = entries
            .iter()
            .map(|e| {
                // Applicability is part of what a lesson is about: a query can
                // name the environment without repeating the lesson body.
                // `invalid_if` is deliberately excluded: it states when the
                // lesson is no longer true, so indexing it would make the
                // stale condition a positive match.
                let experience = e
                    .experience
                    .as_ref()
                    .map(|record| record.applies_when.as_str())
                    .unwrap_or_default();
                format!("{} {} {} {}", e.title, e.body, e.tags.join(" "), experience)
            })
            .collect();
        let docs: Vec<Vec<String>> = doc_texts.iter().map(|text| tokenize(text)).collect();
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
            // Lexical overlap is retrieval, not applicability. The caller
            // bounds this candidate set and the main model judges which
            // candidates this task actually needs; the store must not encode
            // "relevant" as a word list or phrase rule.
            if score > 0.0 {
                scored.push((i, score * relevance_weight(&entries[i])));
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

/// Bounded prior for automatic recall.
///
/// BM25 answers "does this text match"; it cannot tell a user-stated project
/// fact from a model guess or a mechanically verified lesson. This factor
/// breaks near-ties using what the store already knows. It is deliberately
/// small: an unrelated authoritative entry must not outrank a strongly matching
/// weaker one, so the lexical score remains the dominant term.
fn relevance_weight(entry: &MemoryEntry) -> f64 {
    let authority = match entry.authority() {
        MemoryAuthority::ExplicitUser => 1.20,
        MemoryAuthority::ProjectSource => 1.10,
        MemoryAuthority::RuntimeObservation => 1.02,
        MemoryAuthority::ToolObservation => 1.00,
        MemoryAuthority::ModelInference => 0.95,
    };
    let scope = match entry.scope {
        MemoryScope::Project => 1.0,
        MemoryScope::Ephemeral => 0.90,
    };
    let evidence = match entry.experience.as_ref().map(ExperienceRecord::standing) {
        Some(ExperienceStanding::VerifiedResolution) => 1.15,
        Some(ExperienceStanding::Observed) => 1.02,
        Some(ExperienceStanding::Inferred) => 0.95,
        None => 1.0,
    };
    authority * scope * evidence
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
        value: None,
        kind: None,
        status: MemoryStatus::Active,
        scope: MemoryScope::Project,
        provenance: None,
        supersedes: None,
        superseded_by: None,
        superseded_at: None,
        expires_at: None,
        experience: None,
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
    // The structured record is the authority for what this entry IS. A
    // candidate that carries it is an experience even when its free-form
    // candidate kind is `Free` (which only exists to serve the lifecycle).
    if candidate.experience.is_some() {
        entry.kind = Some(EXPERIENCE_KIND.to_string());
    }
    // A caller that means `decision`/`note` records it in the tags; the
    // candidate category alone cannot tell those apart from a generic fact.
    let declared_kind = candidate
        .tags
        .iter()
        .find_map(|tag| tag.trim().strip_prefix("kind:"))
        .map(str::trim)
        .filter(|kind| matches!(*kind, "preference" | "decision" | "note"));
    if entry.kind.as_deref() != Some(EXPERIENCE_KIND) {
        entry.kind = Some(declared_kind.map(str::to_string).unwrap_or_else(|| {
            match candidate.kind {
                CandidateKind::Preference => "preference",
                CandidateKind::PackageManager => "package_manager",
                CandidateKind::Free => "free",
            }
            .to_string()
        }));
    }
    entry.provenance = Some(Provenance {
        authority: candidate.authority,
        source: match candidate.source {
            crate::CandidateSource::UserExplicit => "explicit_user",
            crate::CandidateSource::SystemPropose => "system_propose",
            crate::CandidateSource::SystemInferred => "system_inferred",
            crate::CandidateSource::AgentProposed => "agent_remember_tool",
            crate::CandidateSource::UserRestore => "user_restore",
            crate::CandidateSource::UserAccepted => "user_accepted",
            crate::CandidateSource::RuntimeObserved => "agent_experience",
        }
        .to_string(),
        evidence: None,
    });
    entry.expires_at = candidate.expires_at.clone();
    entry.experience = candidate.experience.clone();
    entry.value = candidate.value.clone();
    entry
}

/// Kind/key labels that mark an entry as a fact read out of the repository
/// rather than something a person decided. Kept as data so existing entries
/// stay readable; excluded from automatic injection so a stale copy can never
/// contradict the working tree.
const DERIVED_LABELS: [&str; 1] = ["package_manager"];

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
    /// Evidence-bound knowledge learned from execution. Recalled by relevance,
    /// never injected as a standing rule and never listed in the catalog.
    Experience,
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
            Self::Experience => EXPERIENCE_KIND,
        }
    }

    /// The candidate category that carries this kind through the lifecycle.
    /// `preference` stays distinct because it is injected every turn; the
    /// decision/note distinction is preserved by a `kind:` tag instead.
    pub fn candidate_kind(self) -> CandidateKind {
        match self {
            Self::Preference => CandidateKind::Preference,
            Self::Derived => CandidateKind::PackageManager,
            Self::Decision | Self::Note | Self::LegacyUnknown | Self::Experience => {
                CandidateKind::Free
            }
        }
    }

    /// Parse a stored entry's labels. `key` is consulted because older derived
    /// entries carry the marker there and not in `kind`.
    pub fn of(entry: &MemoryEntry) -> Self {
        if is_derived_fact(entry) {
            return Self::Derived;
        }
        // The structured record is authoritative even if a legacy/foreign
        // writer labelled the kind differently. An entry that carries the
        // evidence contract IS an experience.
        if entry.experience.is_some() {
            return Self::Experience;
        }
        match entry.kind.as_deref().map(str::trim) {
            Some("preference") => Self::Preference,
            Some("decision") => Self::Decision,
            Some("note") => Self::Note,
            // The stored label is the public contract; `EXPERIENCE_KIND` is
            // the same value for writers.
            Some("experience") => Self::Experience,
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
    if let Some(experience) = entry.experience.as_ref()
        && (looks_like_secret(&experience.applies_when)
            || experience
                .invalid_if
                .as_deref()
                .is_some_and(looks_like_secret)
            || experience.evidence.iter().any(|evidence| {
                looks_like_secret(&evidence.kind) || looks_like_secret(&evidence.reference)
            }))
    {
        fields.push("experience");
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
    if let Some(experience) = entry.experience.as_mut() {
        // An experience's conditions and evidence are durable content too.
        // A credential-shaped one means there is nothing safe to keep.
        if looks_like_secret(&experience.applies_when) {
            experience.applies_when = REDACTED_BODY.to_string();
        }
        if experience
            .invalid_if
            .as_deref()
            .is_some_and(looks_like_secret)
        {
            experience.invalid_if = None;
        }
        experience.evidence.retain(|evidence| {
            !looks_like_secret(&evidence.kind) && !looks_like_secret(&evidence.reference)
        });
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
        let original = std::mem::take(word);
        if original.chars().count() <= 1 {
            return;
        }
        let whole = original.to_lowercase();
        out.push(whole.clone());
        // A code identifier is often written as one word (`PermissionProfile`,
        // `SessionId`). A query says "permission profile"; without the parts the
        // entry can never match. Keep the whole word too, so an exact
        // identifier query still ranks above a partial one.
        let parts = camel_parts(&original);
        if parts.len() > 1 {
            for part in parts {
                let part = part.to_lowercase();
                if part.chars().count() > 1 && part != whole {
                    out.push(part);
                }
            }
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
            // Keep the original case until flush: the camel boundary is the
            // only place it matters.
            word.push(c);
        } else {
            flush_word(&mut word, &mut out);
            flush_cjk(&mut cjk, &mut out);
        }
    }
    flush_word(&mut word, &mut out);
    flush_cjk(&mut cjk, &mut out);
    out
}

/// Split a code identifier at lower/digit → upper boundaries. `SQLite` and
/// `HTTP2` stay one part; `PermissionProfile` becomes `Permission`, `Profile`.
fn camel_parts(word: &str) -> Vec<String> {
    let mut parts = Vec::new();
    let mut current = String::new();
    let mut previous_was_lower_or_digit = false;
    for character in word.chars() {
        if character.is_uppercase() && previous_was_lower_or_digit && !current.is_empty() {
            parts.push(std::mem::take(&mut current));
        }
        previous_was_lower_or_digit = character.is_lowercase() || character.is_ascii_digit();
        current.push(character);
    }
    if !current.is_empty() {
        parts.push(current);
    }
    parts
}

pub(crate) fn now_rfc3339() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
}

pub(crate) fn write_atomically_pub(path: &Path, bytes: &[u8]) -> Result<(), MemoryError> {
    write_atomically(path, bytes)
}

#[cfg(test)]
mod tests {
    /// A structured durable fact. The model owns the subject; the test
    /// supplies it directly, so no prose is parsed into identity anywhere.
    pub(crate) fn fact(subject: &str, body: &str) -> MemoryCandidate {
        let key = semantic_key_of(&normalize_subject(subject));
        let mut candidate = MemoryCandidate::new(
            body,
            body,
            CandidateKind::Free,
            Some(key.clone()),
            CandidateSource::UserExplicit,
            vec![format!("subject:{subject}")],
        )
        .unwrap();
        candidate.semantic_key = Some(key);
        candidate.authority = MemoryAuthority::ExplicitUser;
        candidate
    }

    /// A pending proposal with no parsed meaning.
    fn inferred(body: &str) -> MemoryCandidate {
        MemoryCandidate::new(
            body,
            body,
            CandidateKind::Free,
            None,
            CandidateSource::SystemInferred,
            vec![],
        )
        .unwrap()
    }

    /// One fact must not end up as a direct write AND a candidate awaiting
    /// consent: the user would be asked to approve what is already saved.
    #[test]
    fn one_fact_yields_one_active_and_no_pending_residue() {
        let dir = tempfile::tempdir().unwrap();
        let store = MemoryStore::open(dir.path()).unwrap();
        let body = "提交前先运行 pnpm lint";

        let candidate = match store.propose(inferred(body)).unwrap() {
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
        let candidate = match store.propose(inferred(body)).unwrap() {
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
        let text = "提交前先跑 lint";
        let candidate = match store.propose(inferred(text)).unwrap() {
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

    // ── Agent experience ────────────────────────────────────────────────

    fn verified_experience(subject: &str, body: &str, resolution: &str) -> crate::MemoryCandidate {
        let record = crate::ExperienceRecord {
            category: crate::ExperienceCategory::RootCause,
            verification: crate::ExperienceVerification::Verified,
            applies_when: "cargo test 在 dev profile 下运行".to_string(),
            invalid_if: Some("toolchain 或 dev profile 配置变化".to_string()),
            evidence: vec![
                crate::ExperienceEvidence {
                    kind: "failure".to_string(),
                    reference: "step 2: cargo test -p demo (exit 101)".to_string(),
                },
                crate::ExperienceEvidence {
                    kind: "resolution".to_string(),
                    reference: format!("step 5: {resolution}"),
                },
            ],
        };
        let key = crate::semantic_key_of(subject);
        let mut candidate = crate::MemoryCandidate::new(
            "test 失败源于 dev profile 缺少 feature",
            body,
            crate::CandidateKind::Free,
            Some(key.clone()),
            crate::CandidateSource::RuntimeObserved,
            vec![
                "experience".to_string(),
                "category:root_cause".to_string(),
                format!("subject:{subject}"),
            ],
        )
        .unwrap();
        candidate.semantic_key = Some(key);
        candidate.authority = crate::MemoryAuthority::ToolObservation;
        candidate.evidence = Some(format!("task turn-1; {resolution}"));
        candidate.experience = Some(record);
        candidate
    }

    fn observation_experience(
        subject: &str,
        body: &str,
        observation: &str,
    ) -> crate::MemoryCandidate {
        let record = crate::ExperienceRecord {
            category: crate::ExperienceCategory::EnvironmentConstraint,
            verification: crate::ExperienceVerification::Verified,
            applies_when: "running the project verification script".to_string(),
            invalid_if: Some("the verification script or executor changes".to_string()),
            evidence: vec![crate::ExperienceEvidence {
                kind: "observation".to_string(),
                reference: observation.to_string(),
            }],
        };
        let key = crate::semantic_key_of(subject);
        let mut candidate = crate::MemoryCandidate::new(
            "verification script is silent",
            body,
            crate::CandidateKind::Free,
            Some(key.clone()),
            crate::CandidateSource::RuntimeObserved,
            vec![
                "experience".to_string(),
                "category:environment_constraint".to_string(),
                format!("subject:{subject}"),
            ],
        )
        .unwrap();
        candidate.semantic_key = Some(key);
        candidate.authority = crate::MemoryAuthority::ToolObservation;
        candidate.evidence = Some(format!("task turn-1; {observation}"));
        candidate.experience = Some(record);
        candidate
    }

    /// An observation-only record verifies a fact, not the model's advice. A
    /// later claim under the same identity can read the same output the other
    /// way; silently replacing the safe lesson would make the wrong reading the
    /// only current truth. The person decides.
    #[test]
    fn a_later_claim_cannot_silently_replace_an_observation_only_lesson() {
        let dir = tempfile::tempdir().unwrap();
        let store = MemoryStore::open(dir.path()).unwrap();
        let safe_body = "verify.sh exits 0 with no output; do not treat the exit status as proof; inspect the receipt.";
        let outcome = store
            .admit_experience_candidate(observation_experience(
                "verify.script",
                safe_body,
                "step 1: ./verify.sh exited 0 with no output",
            ))
            .unwrap();
        assert!(matches!(outcome, crate::AgentAdmitOutcome::Applied(_)));

        let wrong = verified_experience(
            "verify.script",
            "verify.sh exits 0 with no output; treat the exit status as the success signal and rely on it.",
            "./verify.sh --engine current (exit 0)",
        );
        let outcome = store.admit_experience_candidate(wrong).unwrap();
        let crate::AgentAdmitOutcome::Queued { candidate, .. } = outcome else {
            panic!("an observation-only lesson must not be silently replaced: {outcome:?}");
        };
        let active = store.effective_active().unwrap();
        assert_eq!(active.len(), 1);
        assert!(
            active[0]
                .body
                .contains("do not treat the exit status as proof"),
            "the safe observation-only lesson must remain current: {active:?}"
        );

        // A person may choose the later reading.
        let accepted = store.accept(&candidate.id).unwrap();
        assert!(accepted.body.contains("success signal"));
        let active = store.effective_active().unwrap();
        assert_eq!(active.len(), 1);
        assert_eq!(active[0].id, accepted.id);
    }

    #[test]
    fn a_verified_experience_is_neither_a_preference_nor_a_catalog_entry() {
        let dir = tempfile::tempdir().unwrap();
        let store = MemoryStore::open(dir.path()).unwrap();
        let candidate = verified_experience(
            "cargo.test.dev_profile",
            "cargo test 报 missing feature 时，先检查 dev profile 的 features；这不是代码错误。",
            "cargo test -p demo (exit 0)",
        );
        let outcome = store.admit_experience_candidate(candidate).unwrap();
        assert!(matches!(outcome, crate::AgentAdmitOutcome::Applied(_)));

        let active = store.effective_active().unwrap();
        assert_eq!(active.len(), 1);
        assert_eq!(
            crate::MemoryKind::of(&active[0]),
            crate::MemoryKind::Experience
        );
        assert!(
            store.standing_preferences(10).unwrap().is_empty(),
            "experience must never be injected as a standing rule"
        );
        assert!(
            store.catalog_lines(10).unwrap().is_empty(),
            "experience must not occupy the per-request catalog"
        );
    }

    #[test]
    fn an_unverified_experience_waits_for_a_person() {
        let dir = tempfile::tempdir().unwrap();
        let store = MemoryStore::open(dir.path()).unwrap();
        let mut candidate = verified_experience(
            "cargo.test.dev_profile",
            "可能是 dev profile 的 feature 缺失导致测试失败。",
            "cargo test -p demo (exit 0)",
        );
        candidate.experience.as_mut().unwrap().verification =
            crate::ExperienceVerification::Unverified;
        let outcome = store.admit_experience_candidate(candidate).unwrap();
        assert!(
            matches!(outcome, crate::AgentAdmitOutcome::Queued { .. }),
            "an unverified experience must be parked: {outcome:?}"
        );
        assert!(
            store.effective_active().unwrap().is_empty(),
            "no unverified experience may become current automatically"
        );
        assert_eq!(store.list_pending().unwrap().len(), 1);
    }

    #[test]
    fn new_evidence_supersedes_the_previous_experience_and_keeps_history() {
        let dir = tempfile::tempdir().unwrap();
        let store = MemoryStore::open(dir.path()).unwrap();
        let first = store
            .admit_experience_candidate(verified_experience(
                "cargo.test.dev_profile",
                "dev profile 缺少 test feature 是根因。",
                "cargo test -p demo (exit 0)",
            ))
            .unwrap();
        assert!(matches!(first, crate::AgentAdmitOutcome::Applied(_)));

        let second = store
            .admit_experience_candidate(verified_experience(
                "cargo.test.dev_profile",
                "真正的原因是 workspace 的 default-features=false；dev profile 只是表象。",
                "cargo test -p demo --workspace (exit 0)",
            ))
            .unwrap();
        match second {
            crate::AgentAdmitOutcome::Applied(outcome) => {
                assert_eq!(outcome.operation, crate::AppliedOperation::Superseded);
            }
            other => panic!("new evidence must update the old experience: {other:?}"),
        }
        let active = store.effective_active().unwrap();
        assert_eq!(active.len(), 1, "one current version per identity");
        assert!(active[0].body.contains("default-features"));
        let archived = store.list_archived().unwrap();
        assert_eq!(archived.len(), 1);
        assert_eq!(archived[0].status, crate::MemoryStatus::Superseded);
    }

    /// The model declares the subject, and the runtime does not second-guess
    /// that declaration with a prose-similarity gate. A reading that names the
    /// same identity and carries verified evidence is the newer account; the
    /// old one is archived, never deleted.
    #[test]
    fn a_verified_reading_under_the_same_identity_becomes_current() {
        let dir = tempfile::tempdir().unwrap();
        let store = MemoryStore::open(dir.path()).unwrap();
        store
            .admit_experience_candidate(verified_experience(
                "cargo.test.assertion",
                "The linker is missing because the Xcode command line tools are not installed; install them before building.",
                "cargo test -p demo (exit 0)",
            ))
            .unwrap();
        let mut conflicting = verified_experience(
            "cargo.test.assertion",
            "The assertion compares bytes and chars after the API changed; update the expected value in the test helper.",
            "cargo test -p demo (exit 0)",
        );
        conflicting.experience.as_mut().unwrap().applies_when =
            "a Rust test asserts on a value produced by the changed API".to_string();
        let outcome = store.admit_experience_candidate(conflicting).unwrap();
        assert!(
            matches!(
                outcome,
                crate::AgentAdmitOutcome::Applied(CommitOutcome {
                    operation: crate::AppliedOperation::Superseded,
                    ..
                })
            ),
            "a verified reading of the same identity is an update: {outcome:?}"
        );
        let active = store.effective_active().unwrap();
        assert_eq!(active.len(), 1);
        assert!(active[0].body.contains("assertion"), "{active:?}");
        assert!(
            store
                .list_archived()
                .unwrap()
                .iter()
                .any(|entry| entry.body.contains("linker")
                    && entry.status == crate::MemoryStatus::Superseded),
            "the replaced account stays as history"
        );
    }

    /// An observation-only reading verifies a fact, not a resolution. It is
    /// weaker evidence than a failure that later passed under the same check,
    /// so it must not silently become the only current account. The person may
    /// still choose it.
    #[test]
    fn a_weaker_observation_cannot_replace_a_verified_resolution() {
        let dir = tempfile::tempdir().unwrap();
        let store = MemoryStore::open(dir.path()).unwrap();
        store
            .admit_experience_candidate(verified_experience(
                "cargo.test.dev_profile",
                "dev profile 缺少 test feature 是根因；启用后测试通过。",
                "cargo test -p demo (exit 0)",
            ))
            .unwrap();

        let outcome = store
            .admit_experience_candidate(observation_experience(
                "cargo.test.dev_profile",
                "dev profile 缺少 test feature 并不成立；只是脚本没有输出。",
                "step 1: ./verify.sh exited 0 with no output",
            ))
            .unwrap();
        let crate::AgentAdmitOutcome::Queued { candidate, .. } = outcome else {
            panic!("weaker evidence must not replace a verified resolution: {outcome:?}");
        };
        let active = store.effective_active().unwrap();
        assert_eq!(active.len(), 1);
        assert!(
            active[0].body.contains("启用后测试通过"),
            "the verified resolution must remain current: {active:?}"
        );

        // Consent still lets the person choose the weaker reading.
        let accepted = store.accept(&candidate.id).unwrap();
        assert!(accepted.body.contains("并不成立"), "{accepted:?}");
        assert_eq!(store.effective_active().unwrap()[0].id, accepted.id);
    }

    /// An unverified claim that a lesson is inapplicable is a hypothesis, not
    /// runtime evidence. The structured verification field decides this, not
    /// the wording of the sentence: only a person may retire the lesson.
    #[test]
    fn an_unverified_claim_that_a_lesson_is_inapplicable_waits_for_a_person() {
        let dir = tempfile::tempdir().unwrap();
        let store = MemoryStore::open(dir.path()).unwrap();
        store
            .admit_experience_candidate(verified_experience(
                "cargo.test.dev_profile",
                "dev profile 缺少 test feature 是根因，启用之后测试就通过了。",
                "cargo test -p demo (exit 0)",
            ))
            .unwrap();

        let mut claim = verified_experience(
            "cargo.test.dev_profile",
            "旧经验已经不适用：缓存目录权限才是真正原因，请先清理缓存重建索引。",
            "cargo test -p demo (exit 0)",
        );
        claim.experience.as_mut().unwrap().verification = crate::ExperienceVerification::Unverified;
        let outcome = store.admit_experience_candidate(claim).unwrap();

        assert!(
            matches!(outcome, crate::AgentAdmitOutcome::Queued { .. }),
            "an unverified claim must not mechanically invalidate an experience: {outcome:?}"
        );
        let active = store.effective_active().unwrap();
        assert_eq!(active.len(), 1);
        assert!(active[0].body.contains("缺少 test feature 是根因"));
    }

    /// Confirming evidence under the same identity is not a second lesson.
    #[test]
    fn confirming_evidence_does_not_create_a_second_current_experience() {
        let dir = tempfile::tempdir().unwrap();
        let store = MemoryStore::open(dir.path()).unwrap();
        let first = verified_experience(
            "cargo.test.dev_profile",
            "dev profile 缺少 test feature 是根因。",
            "cargo test -p demo (exit 0)",
        );
        assert!(matches!(
            store.admit_experience_candidate(first.clone()).unwrap(),
            crate::AgentAdmitOutcome::Applied(_)
        ));
        assert!(matches!(
            store.admit_experience_candidate(first).unwrap(),
            crate::AgentAdmitOutcome::NotStored { .. }
        ));
        assert_eq!(store.effective_active().unwrap().len(), 1);
        assert!(store.list_archived().unwrap().is_empty());
    }

    /// A replaced lesson is history, not current truth: a reopened store (a
    /// new session) recalls the latest version and can audit the link from the
    /// archived original to its replacement.
    #[test]
    fn the_latest_experience_is_what_a_reopened_store_recalls() {
        let dir = tempfile::tempdir().unwrap();
        let store = MemoryStore::open(dir.path()).unwrap();
        let old = match store
            .admit_experience_candidate(verified_experience(
                "cargo.test.dev_profile",
                "dev profile 缺少 test feature 是根因。",
                "cargo test -p demo (exit 0)",
            ))
            .unwrap()
        {
            crate::AgentAdmitOutcome::Applied(outcome) => outcome.entry.unwrap(),
            other => panic!("the first lesson must be admitted: {other:?}"),
        };
        let new = match store
            .admit_experience_candidate(verified_experience(
                "cargo.test.dev_profile",
                "真正的原因是 workspace 的 default-features=false。",
                "cargo test -p demo --workspace (exit 0)",
            ))
            .unwrap()
        {
            crate::AgentAdmitOutcome::Applied(outcome) => outcome.entry.unwrap(),
            other => panic!("new evidence must replace the old lesson: {other:?}"),
        };
        assert_eq!(new.supersedes.as_deref(), Some(old.id.as_str()));
        drop(store);

        let reopened = MemoryStore::open(dir.path()).unwrap();
        let active = reopened.effective_active().unwrap();
        assert_eq!(active.len(), 1);
        assert_eq!(active[0].id, new.id);
        assert!(active[0].body.contains("default-features"));
        assert!(
            reopened
                .search("dev profile 缺少 test feature", 10)
                .unwrap()
                .into_iter()
                .all(|(entry, _)| entry.id != old.id),
            "a superseded lesson must never be recalled as current past"
        );
        let archived = reopened
            .list_archived()
            .unwrap()
            .into_iter()
            .find(|entry| entry.id == old.id)
            .expect("the original stays as history");
        assert_eq!(archived.status, crate::MemoryStatus::Superseded);
        assert_eq!(archived.superseded_by.as_deref(), Some(new.id.as_str()));
        assert!(archived.superseded_at.is_some());
    }

    /// Experience identity is project-local. Two projects may hold the same
    /// failure signature with different current lessons.
    #[test]
    fn separate_projects_keep_their_own_experience() {
        let dir_a = tempfile::tempdir().unwrap();
        let dir_b = tempfile::tempdir().unwrap();
        let store_a = MemoryStore::open(dir_a.path()).unwrap();
        let store_b = MemoryStore::open(dir_b.path()).unwrap();
        store_a
            .admit_experience_candidate(verified_experience(
                "cargo.test.dev_profile",
                "A 项目：dev profile 缺少 test feature。",
                "cargo test (exit 0)",
            ))
            .unwrap();
        store_b
            .admit_experience_candidate(verified_experience(
                "cargo.test.dev_profile",
                "B 项目：链接器缺少系统工具链。",
                "cargo test (exit 0)",
            ))
            .unwrap();

        let hits = store_a.search("dev profile 链接器", 10).unwrap();
        assert_eq!(hits.len(), 1);
        assert!(hits[0].0.body.contains("A 项目"));
        assert_eq!(store_b.effective_active().unwrap().len(), 1);
        assert!(
            store_b.effective_active().unwrap()[0]
                .body
                .contains("B 项目")
        );
    }

    #[test]
    fn an_experience_cannot_replace_a_fact_of_another_kind() {
        let dir = tempfile::tempdir().unwrap();
        let store = MemoryStore::open(dir.path()).unwrap();
        // A user-authored decision under the same semantic identity.
        let key = crate::semantic_key_of("项目测试命令");
        let mut entry = crate::new_entry(
            "项目测试命令",
            "cargo test -p demo 是唯一验收命令。",
            vec![],
        );
        entry.id = "project-test-command".to_string();
        entry.key = Some(key.clone());
        entry.kind = Some("decision".to_string());
        entry.provenance = Some(crate::Provenance {
            authority: crate::MemoryAuthority::ExplicitUser,
            source: "user_direct".to_string(),
            evidence: None,
        });
        store.remember(entry).unwrap();

        let candidate = verified_experience(
            "项目测试命令",
            "跑 cargo test 前必须先设置 dev profile。",
            "cargo test -p demo (exit 0)",
        );
        let outcome = store.admit_experience_candidate(candidate).unwrap();
        assert!(
            matches!(outcome, crate::AgentAdmitOutcome::Queued { .. }),
            "an experience must not silently replace a user fact: {outcome:?}"
        );
        let active = store.effective_active().unwrap();
        assert_eq!(active.len(), 1);
        assert_eq!(active[0].kind.as_deref(), Some("decision"));
        assert_eq!(
            active[0].authority(),
            crate::MemoryAuthority::ExplicitUser,
            "the user fact remains the current truth"
        );
    }

    #[test]
    fn recall_matches_chinese_english_and_code_identifiers_together() {
        let dir = tempfile::tempdir().unwrap();
        let store = MemoryStore::open(dir.path()).unwrap();
        let mut entry = crate::new_entry(
            "PermissionProfile 校验失败",
            "PermissionProfile::Assisted 在 workspace 写入时仍会调用 admission；\
             cargo nextest run --workspace 能复现。",
            vec!["experience".to_string()],
        );
        entry.kind = Some("experience".to_string());
        entry.experience = Some(crate::ExperienceRecord {
            category: crate::ExperienceCategory::FailurePattern,
            verification: crate::ExperienceVerification::Verified,
            applies_when: "workspace 写入且 permission 为 Assisted".to_string(),
            invalid_if: None,
            evidence: vec![
                crate::ExperienceEvidence {
                    kind: "failure".to_string(),
                    reference: "step 3: cargo nextest run --workspace (exit 101)".to_string(),
                },
                crate::ExperienceEvidence {
                    kind: "resolution".to_string(),
                    reference: "step 6: cargo nextest run --workspace (exit 0)".to_string(),
                },
            ],
        });
        store.remember(entry).unwrap();

        for query in [
            "permission profile 校验失败",
            "PermissionProfile Assisted 为什么失败",
            "cargo nextest workspace 失败",
            "权限 workspace 写入",
        ] {
            assert!(
                !store.recall(query, 5).unwrap().is_empty(),
                "recall missed a mixed-language query: {query}"
            );
        }
    }

    #[test]
    fn a_person_accepting_an_experience_may_replace_another_kind() {
        let dir = tempfile::tempdir().unwrap();
        let store = MemoryStore::open(dir.path()).unwrap();
        let subject = "workspace.marker";
        let key = crate::semantic_key_of(subject);
        let mut decision =
            crate::new_entry("workspace 规则", "marker.txt 必须由人工创建。", vec![]);
        decision.id = "workspace-marker-rule".to_string();
        decision.key = Some(key.clone());
        decision.kind = Some("decision".to_string());
        decision.provenance = Some(crate::Provenance {
            authority: crate::MemoryAuthority::ExplicitUser,
            source: "user_direct".to_string(),
            evidence: None,
        });
        store.remember(decision).unwrap();

        let candidate = verified_experience(
            subject,
            "检查依赖 marker.txt；文件缺失时失败，创建后通过。",
            "cargo test -p demo (exit 0)",
        );
        let queued = store.admit_experience_candidate(candidate).unwrap();
        let crate::AgentAdmitOutcome::Queued { candidate, .. } = queued else {
            panic!("a conflicting kind must wait: {queued:?}");
        };
        assert_eq!(store.list_pending().unwrap().len(), 1);

        let accepted = store.accept(&candidate.id).unwrap();
        assert_eq!(
            crate::MemoryKind::of(&accepted),
            crate::MemoryKind::Experience
        );
        assert_eq!(
            accepted.authority(),
            crate::MemoryAuthority::ToolObservation,
            "consent does not raise the evidence class"
        );
        let active = store.effective_active().unwrap();
        assert_eq!(active.len(), 1);
        assert_eq!(active[0].id, accepted.id);
        assert!(
            store
                .list_archived()
                .unwrap()
                .iter()
                .any(|entry| entry.id == "workspace-marker-rule"
                    && entry.status == crate::MemoryStatus::Superseded),
            "the replaced decision stays as history"
        );
    }

    #[test]
    fn recall_matches_an_experience_by_its_applicability_condition() {
        let dir = tempfile::tempdir().unwrap();
        let store = MemoryStore::open(dir.path()).unwrap();
        let mut candidate = verified_experience(
            "build.linker",
            "最终链接失败时检查系统 C 工具链。",
            "cargo test (exit 0)",
        );
        candidate.experience.as_mut().unwrap().applies_when =
            "容器里没有 build-essential 或 cc 不在 PATH".to_string();
        store.admit_experience_candidate(candidate).unwrap();
        assert!(
            !store.recall("build-essential 容器", 5).unwrap().is_empty(),
            "a query naming only the applicability must find the lesson"
        );
    }

    /// An invalidation condition describes when a lesson STOPS applying. It
    /// must not make the lesson a positive match for that very condition: a
    /// query about the stale state would otherwise surface the entry as if it
    /// recommended the stale behaviour.
    #[test]
    fn an_invalidation_condition_is_not_positive_recall_material() {
        let dir = tempfile::tempdir().unwrap();
        let store = MemoryStore::open(dir.path()).unwrap();
        let mut candidate = verified_experience(
            "build.linker",
            "链接失败时先检查系统 C 工具链。",
            "cargo test (exit 0)",
        );
        candidate.experience.as_mut().unwrap().invalid_if = Some("仓库迁移到 bun 10".to_string());
        store.admit_experience_candidate(candidate).unwrap();
        assert!(
            store.recall("bun", 5).unwrap().is_empty(),
            "the invalidation condition must not act as a positive match"
        );
    }

    #[test]
    fn an_experience_cannot_be_written_without_its_evidence() {
        let dir = tempfile::tempdir().unwrap();
        let store = MemoryStore::open(dir.path()).unwrap();
        let error = store
            .activate(
                "看起来像经验",
                "没有证据的教训。",
                crate::MemoryKind::Experience,
                vec![],
            )
            .unwrap_err();
        assert!(error.to_string().contains("runtime evidence"), "{error}");
        assert!(store.effective_active().unwrap().is_empty());
    }

    #[test]
    fn an_expired_experience_is_neither_current_nor_recalled() {
        let dir = tempfile::tempdir().unwrap();
        let store = MemoryStore::open(dir.path()).unwrap();
        let mut candidate = verified_experience(
            "cargo.test.dev_profile",
            "dev profile 缺少 test feature 是根因；启用后测试通过。",
            "cargo test -p demo (exit 0)",
        );
        candidate.expires_at = Some("2000-01-01T00:00:00Z".to_string());
        let outcome = store.admit_experience_candidate(candidate).unwrap();
        assert!(matches!(outcome, crate::AgentAdmitOutcome::Applied(_)));
        assert!(
            store.effective_active().unwrap().is_empty(),
            "an expired experience is not current truth"
        );
        assert!(
            store
                .recall("cargo test dev profile", 5)
                .unwrap()
                .is_empty(),
            "an expired experience must not reach a later task"
        );
    }

    #[test]
    fn a_sensitive_experience_is_refused_at_the_write_boundary() {
        let dir = tempfile::tempdir().unwrap();
        let store = MemoryStore::open(dir.path()).unwrap();
        let mut candidate = verified_experience(
            "deploy.token",
            "部署失败时检查 token。",
            "cargo test -p demo (exit 0)",
        );
        candidate.experience.as_mut().unwrap().applies_when =
            "token=ghp_abcdefghijklmnopqrstuvwxyz0123".to_string();
        let outcome = store.admit_experience_candidate(candidate);
        assert!(outcome.is_err(), "a credential in evidence must be refused");
        assert!(store.effective_active().unwrap().is_empty());
    }

    #[test]
    fn recall_can_be_restricted_to_one_lane() {
        let dir = tempfile::tempdir().unwrap();
        let store = MemoryStore::open(dir.path()).unwrap();
        let mut note = new_entry(
            "Deploy target",
            "The blue deployment target is the one this project uses.",
            Vec::new(),
        );
        note.kind = Some("note".to_string());
        store.remember(note).unwrap();
        let candidate = verified_experience(
            "deploy.target",
            "The blue deployment target is the one this project uses.",
            "cargo test -p demo (exit 0)",
        );
        store.admit_experience_candidate(candidate).unwrap();

        let user = store
            .recall_for_lanes("blue deployment target", 4, MemoryLanes::USER_ONLY)
            .unwrap();
        assert!(!user.is_empty());
        assert!(user.iter().all(|(entry, _)| entry.experience.is_none()));

        let experience = store
            .recall_for_lanes("blue deployment target", 4, MemoryLanes::EXPERIENCE_ONLY)
            .unwrap();
        assert!(!experience.is_empty());
        assert!(
            experience
                .iter()
                .all(|(entry, _)| entry.experience.is_some())
        );

        assert_eq!(
            store
                .recall_for_lanes("blue deployment target", 10, MemoryLanes::ALL)
                .unwrap()
                .len(),
            2
        );
        assert!(
            store
                .recall_for_lanes("blue deployment target", 10, MemoryLanes::NONE)
                .unwrap()
                .is_empty()
        );
    }

    /// BM25 alone would rank the shorter, weaker entry first: lexical overlap
    /// is not the whole of relevance. A fact the user stated outranks the
    /// model's own inference when both match the request.
    #[test]
    fn recall_ranks_user_authority_above_inference_at_a_similar_match() {
        let dir = tempfile::tempdir().unwrap();
        let store = MemoryStore::open(dir.path()).unwrap();

        let mut inferred = new_entry("发布命令", "发布前运行 cargo test", vec![]);
        inferred.id = "a-inferred".to_string();
        inferred.provenance = Some(Provenance {
            authority: MemoryAuthority::ModelInference,
            source: "system_inferred".to_string(),
            evidence: None,
        });
        store.remember(inferred).unwrap();

        let mut explicit = new_entry("发布命令", "发布前运行 cargo test，并检查版本号。", vec![]);
        explicit.id = "z-explicit".to_string();
        explicit.provenance = Some(Provenance {
            authority: MemoryAuthority::ExplicitUser,
            source: "explicit_user".to_string(),
            evidence: None,
        });
        store.remember(explicit).unwrap();

        let hits = store.search("发布前运行 cargo test 发布命令", 5).unwrap();
        assert_eq!(
            hits[0].0.id,
            "z-explicit",
            "the user's own fact must lead: {:?}",
            hits.iter()
                .map(|(entry, score)| (&entry.id, score))
                .collect::<Vec<_>>()
        );
    }

    /// A mechanically verified resolution outranks a model explanation of the
    /// same area, even when the explanation has slightly better term overlap.
    #[test]
    fn recall_ranks_verified_experience_above_inference() {
        let dir = tempfile::tempdir().unwrap();
        let store = MemoryStore::open(dir.path()).unwrap();

        let mut inferred = new_entry("测试环境", "dev profile 缺少 test feature", vec![]);
        inferred.id = "a-inferred".to_string();
        inferred.kind = Some(EXPERIENCE_KIND.to_string());
        inferred.experience = Some(ExperienceRecord {
            category: ExperienceCategory::RootCause,
            verification: ExperienceVerification::Unverified,
            applies_when: "dev profile 缺少 test feature".to_string(),
            invalid_if: None,
            evidence: Vec::new(),
        });
        store.remember(inferred).unwrap();

        let mut verified = new_entry(
            "测试环境",
            "dev profile 缺少 test feature；启用后测试通过。",
            vec![],
        );
        verified.id = "z-verified".to_string();
        verified.kind = Some(EXPERIENCE_KIND.to_string());
        verified.experience = Some(ExperienceRecord {
            category: ExperienceCategory::RootCause,
            verification: ExperienceVerification::Verified,
            applies_when: "dev profile 缺少 test feature".to_string(),
            invalid_if: None,
            evidence: vec![
                ExperienceEvidence {
                    kind: "failure".to_string(),
                    reference: "step 1: cargo test (exit 101)".to_string(),
                },
                ExperienceEvidence {
                    kind: "resolution".to_string(),
                    reference: "step 2: cargo test (exit 0)".to_string(),
                },
            ],
        });
        store.remember(verified).unwrap();

        let hits = store.search("dev profile 缺少 test feature", 5).unwrap();
        assert_eq!(
            hits[0].0.id,
            "z-verified",
            "verified evidence must lead: {:?}",
            hits.iter()
                .map(|(entry, score)| (&entry.id, score))
                .collect::<Vec<_>>()
        );
    }

    fn store_with_code_and_mcp() -> (tempfile::TempDir, MemoryStore) {
        let dir = tempfile::tempdir().unwrap();
        let store = MemoryStore::open(dir.path()).unwrap();
        for (id, title) in [
            ("code", "不提交生成的客户端代码"),
            ("mcp", "MCP 读写必须经过身份验证"),
            ("db", "数据库访问：repository 层"),
        ] {
            let mut entry = new_entry(title, title, vec![]);
            entry.id = id.to_string();
            entry.provenance = Some(Provenance {
                authority: MemoryAuthority::ExplicitUser,
                source: "explicit_user".to_string(),
                evidence: None,
            });
            store.remember(entry).unwrap();
        }
        (dir, store)
    }

    /// BM25 retrieval is not a verdict. A long request that shares generic
    /// words with a rule still offers that rule as a candidate; whether it
    /// applies is the main model's judgment, not a curated word list's.
    #[test]
    fn a_generic_word_overlap_is_offered_as_a_candidate() {
        let (_dir, store) = store_with_code_and_mcp();
        let query = "用 Python 写一个生成斐波那契数列前十项的函数，只给代码，不要修改任何文件，也不要读取项目文件。";
        let hits = store.recall(query, 4).unwrap();
        assert!(
            hits.iter().any(|(entry, _)| entry.id == "code"),
            "lexical retrieval must offer the candidate: {:?}",
            hits.iter()
                .map(|(entry, score)| (&entry.id, score))
                .collect::<Vec<_>>()
        );
        assert!(hits.len() <= 4, "the candidate set stays bounded");
    }

    /// The same long request that names the subject still recalls the entry:
    /// the gate drops incidental words, not technical identifiers.
    #[test]
    fn recall_keeps_a_technical_identifier_in_a_long_request() {
        let (_dir, store) = store_with_code_and_mcp();
        let hits = store
            .recall(
                "实现新的 MCP 查询接口。只给出实现方案，不要修改文件；说明需要遵守的项目约束。",
                4,
            )
            .unwrap();
        assert_eq!(
            hits.iter()
                .map(|(entry, _)| entry.id.as_str())
                .collect::<Vec<_>>(),
            vec!["mcp"],
            "a named technical subject must still be recalled"
        );
    }

    /// A short query made almost entirely of one Chinese word is a real match:
    /// the gate must not turn into a length filter.
    #[test]
    fn recall_keeps_a_short_chinese_topic_query() {
        let dir = tempfile::tempdir().unwrap();
        let store = MemoryStore::open(dir.path()).unwrap();
        let entry = new_entry("部署密钥保管人", "由代号 Zephyr-Q7 的同事保管。", vec![]);
        store.remember(entry).unwrap();
        let hits = store.recall("部署", 4).unwrap();
        assert_eq!(hits.len(), 1, "a short topic query was over-filtered");
    }

    /// A user refers to a durable decision by a shorthand that names part of
    /// its declared subject. The sentence's other bigrams are function words,
    /// so the overlap ratio rejects it; the subject identity must not.
    #[test]
    fn recall_recognises_a_shorthand_for_the_declared_subject() {
        let dir = tempfile::tempdir().unwrap();
        let store = MemoryStore::open(dir.path()).unwrap();
        let fact = crate::tests::fact("默认模型", "以后本项目默认模型固定为 flash");
        store.commit_candidate(&fact).unwrap();
        let hits = store.recall("模型按之前定的来", 4).unwrap();
        assert_eq!(hits.len(), 1, "the decision was not recalled: {hits:?}");
        assert!(hits[0].0.body.contains("flash"), "{:?}", hits[0].0.body);
    }

    /// A single shared word produces a candidate row. Whether it names the
    /// same decision is the model's call; the store does not turn that into a
    /// hidden identity rule.
    #[test]
    fn a_single_shared_word_is_a_candidate_not_an_identity_match() {
        let dir = tempfile::tempdir().unwrap();
        let store = MemoryStore::open(dir.path()).unwrap();
        let fact = crate::tests::fact(
            "API 响应字段命名",
            "以后 API 响应字段命名统一使用 snake_case",
        );
        store.commit_candidate(&fact).unwrap();
        let hits = store.recall("这个字段怎么序列化", 4).unwrap();
        assert_eq!(
            hits.len(),
            1,
            "a lexical candidate must be offered for judgment: {hits:?}"
        );
    }

    // The task texts below are the real prompts from the DF-M3 acceptance
    // arms (2026-10-22) whose recall decisions were wrong. They stay verbatim
    // so this regression keeps testing the observed failure, not a simplified
    // paraphrase of it.
    const ENGINE_TASK: &str = r#"Execute `./verify.sh` as one standalone command: no `;`, pipe, redirection, or appended exit-status echo. It exits non-zero with the message `error: engine profile rejected`. Diagnose the real cause in this environment, then fix the project configuration so `./verify.sh` exits 0 and prints an `engine: build ok` receipt. Do not edit `./verify.sh` and do not modify the engine program or anything outside this repository; fix the project's own `engine.config`."#;
    const PYTHON_REPORT_TASK: &str = r#"Run `python3 -m unittest tests.test_report`. It fails with `AssertionError: 30.0 != 15.0` and a `ZeroDivisionError` on the one-value case. Diagnose the real cause in `scripts/report.py` and fix it so both tests pass. Do not change the tests or anything outside this repository."#;
    const CARGO_SCALE_TASK: &str = r#"Run `cargo test --test scale`. It fails because `scale(3, 4)` returns 7 instead of 12. Fix `src/lib.rs` so `cargo test --test scale` passes. Only the scale failure is in scope; do not change the tests or unrelated functions, and do not fix unrelated failing tests."#;
    const PYBUILD_TASK: &str = r#"What layout value does the installed pybuild in this environment accept, and what must `pybuild.config` contain for `./verify.sh` to succeed? Answer from the current environment. Do not modify any files."#;

    fn learn_experience(
        store: &MemoryStore,
        subject: &str,
        title: &str,
        body: &str,
        applies_when: &str,
    ) {
        let mut candidate = verified_experience(subject, body, "cargo test --test scale (exit 0)");
        candidate.title = title.to_string();
        let record = candidate.experience.as_mut().unwrap();
        record.applies_when = applies_when.to_string();
        record.invalid_if = None;
        let outcome = store.admit_experience_candidate(candidate).unwrap();
        assert!(
            matches!(outcome, crate::AgentAdmitOutcome::Applied(_)),
            "fixture lesson was not activated: {outcome:?}"
        );
    }

    fn engine_lesson(store: &MemoryStore) {
        learn_experience(
            store,
            "experience_engine_config_profile_rejected",
            "Align engine.config profile with accepted profile",
            "When `./verify.sh` exits non-zero with `error: engine profile rejected`, inspect and correct the profile selected in the project's `engine.config` rather than editing `./verify.sh` or the engine program. Editing `engine.config` to a supported profile was followed by `./verify.sh` exiting 0 and printing `engine: build ok (profile=legacy)`, so use the accepted profile named in the receipt as the configuration target. If no receipt appears, identify the profile values the engine accepts and match the config to one of them.",
            "`./verify.sh` fails with `error: engine profile rejected` and the project controls its profile through `engine.config`.",
        );
    }

    fn preview_lesson(store: &MemoryStore) {
        learn_experience(
            store,
            "experience_rust_utf8_byte_index_truncation_panic",
            "Byte-index truncation panics on multibyte UTF-8 previews",
            "Preview/truncation code that slices a string at a fixed byte offset (`&s[..n]`) panics on non-ASCII input: the run failed with \"end byte index 4 is not a char boundary; it is inside '文' (bytes 3..6) of `中文预览`\". The handling executed was an edit in src/lib.rs, after which `cargo test --test preview` reported \"test result: ok. 1 passed; 0 failed\". Truncate by characters or by a boundary-safe helper (chars().take(n), char_indices, floor_char_boundary) rather than raw byte indices; that mechanism is inferred, since the diff itself is not in evidence.",
            "truncating, previewing, or slicing user-visible strings that may contain multibyte UTF-8 (CJK, emoji) by a length limit",
        );
    }

    fn pybuild_lesson(store: &MemoryStore) {
        learn_experience(
            store,
            "experience_pybuild_config_layout_mismatch",
            "Match pybuild layout to repository shape",
            "When `./verify.sh` exits 1 with `error: build layout rejected`, check the repository's `pybuild.config` layout/configuration against the actual tree shape rather than editing the verify script or pybuild itself. In the observed repository the accepted configuration produced exit 0 and the receipt `pybuild: build ok (layout=flat)`, and `pybuild.config` was the only modified project file, so the actionable fix is to set or correct the layout value in that config to the layout the project actually uses.",
            "A pybuild-based verification command fails with `error: build layout rejected` and the project owns a `pybuild.config`.",
        );
    }

    /// Real failure from DF-M3: the engine lesson shares `error`, `test`,
    /// `fix` and `project` with a Python report task. BM25 cannot tell a
    /// toolchain word from subject matter, so the lesson is offered as a
    /// candidate; the injected block says candidates are unconfirmed and the
    /// main model decides. The store's job is only to bound the set.
    #[test]
    fn retrieval_offers_an_ecosystem_overlap_as_a_candidate() {
        let dir = tempfile::tempdir().unwrap();
        let store = MemoryStore::open(dir.path()).unwrap();
        engine_lesson(&store);

        let hits = store
            .recall_for_lanes(PYTHON_REPORT_TASK, 2, MemoryLanes::EXPERIENCE_ONLY)
            .unwrap();
        assert!(
            hits.iter()
                .any(|(entry, _)| entry.title.contains("Align engine.config profile")),
            "retrieval must offer the candidate, not apply a word-list verdict: {:?}",
            hits.iter().map(|(entry, _)| &entry.id).collect::<Vec<_>>()
        );
        assert!(hits.len() <= 2, "the candidate set stays bounded");
    }

    /// Real failure from DF-M3: the UTF-8 preview lesson shares `cargo`,
    /// `test`, `src` and `lib` with a cargo scale task. It is a retrieval
    /// candidate, not a runtime verdict about relevance.
    #[test]
    fn retrieval_offers_a_toolchain_overlap_as_a_candidate() {
        let dir = tempfile::tempdir().unwrap();
        let store = MemoryStore::open(dir.path()).unwrap();
        preview_lesson(&store);

        let hits = store
            .recall_for_lanes(CARGO_SCALE_TASK, 2, MemoryLanes::EXPERIENCE_ONLY)
            .unwrap();
        assert!(
            hits.iter()
                .any(|(entry, _)| entry.title.contains("Byte-index truncation")),
            "retrieval must offer the candidate, not apply a word-list verdict: {:?}",
            hits.iter().map(|(entry, _)| &entry.id).collect::<Vec<_>>()
        );
        assert!(hits.len() <= 2, "the candidate set stays bounded");
    }

    /// The same failure described in other words is still the same failure.
    /// Retrieval must follow the defect, not one exact task phrase.
    #[test]
    fn an_experience_matches_the_same_failure_in_different_words() {
        let dir = tempfile::tempdir().unwrap();
        let store = MemoryStore::open(dir.path()).unwrap();
        engine_lesson(&store);

        let paraphrase =
            "verify.sh is rejected by the engine because of the profile in engine.config";
        let hits = store
            .recall_for_lanes(paraphrase, 2, MemoryLanes::EXPERIENCE_ONLY)
            .unwrap();
        assert_eq!(
            hits.len(),
            1,
            "the same failure was not recognised: {hits:?}"
        );
        assert_eq!(
            hits[0].0.title, "Align engine.config profile with accepted profile",
            "a different lesson matched: {hits:?}"
        );

        let fresh_task = r#"The preview helper panics on a multibyte boundary while truncating Chinese text with `cargo test --test preview`."#;
        let dir = tempfile::tempdir().unwrap();
        let store = MemoryStore::open(dir.path()).unwrap();
        preview_lesson(&store);
        let hits = store
            .recall_for_lanes(fresh_task, 2, MemoryLanes::EXPERIENCE_ONLY)
            .unwrap();
        assert_eq!(
            hits.len(),
            1,
            "a new task naming the same defect was missed: {hits:?}"
        );
    }

    /// The ordinary positive: the task that produced the lesson recalls it.
    #[test]
    fn an_experience_matches_the_task_that_established_it() {
        let dir = tempfile::tempdir().unwrap();
        let store = MemoryStore::open(dir.path()).unwrap();
        engine_lesson(&store);
        let hits = store
            .recall_for_lanes(ENGINE_TASK, 2, MemoryLanes::EXPERIENCE_ONLY)
            .unwrap();
        assert_eq!(
            hits.len(),
            1,
            "the task that produced the lesson missed it: {hits:?}"
        );
    }

    /// Same failure family, different root cause: a pybuild layout task ranks
    /// the pybuild lesson first. Ranking is retrieval; the model still judges
    /// whether the lesson applies.
    #[test]
    fn a_verify_failure_ranks_the_lesson_for_its_own_root_cause_first() {
        let dir = tempfile::tempdir().unwrap();
        let store = MemoryStore::open(dir.path()).unwrap();
        engine_lesson(&store);
        pybuild_lesson(&store);

        let hits = store
            .recall_for_lanes(PYBUILD_TASK, 2, MemoryLanes::EXPERIENCE_ONLY)
            .unwrap();
        assert!(hits.len() <= 2, "the candidate set stays bounded");
        assert_eq!(
            hits[0].0.title, "Match pybuild layout to repository shape",
            "a different failure family ranked first: {hits:?}"
        );
    }

    /// Anti-overfit: a task type outside the DF-M3 fixtures. Retrieval must
    /// rank the lesson for the same defect first, and rank a different
    /// problem's lesson first for that problem — without a curated word list.
    #[test]
    fn a_bundler_lesson_ranks_by_defect_not_by_ecosystem() {
        let dir = tempfile::tempdir().unwrap();
        let store = MemoryStore::open(dir.path()).unwrap();
        learn_experience(
            &store,
            "experience_vite_missing_public_asset_alias",
            "Vite bundle fails on a missing public asset after an alias change",
            "When the frontend bundle aborts with `ENOENT` for `public/logo.svg` after an alias change, check that the alias still resolves the public asset; restoring the alias path made `vite build` exit 0.",
            "A Vite build fails with ENOENT on a public asset after an alias or import-path change.",
        );
        learn_experience(
            &store,
            "experience_vite_proxy_ignored",
            "Vite dev server ignores the proxy setting",
            "When the Vite dev server returns 404 for `/api` during local testing, check that the proxy entry sits under `server.proxy` and that the request path still matches its prefix; moving the entry made the dev server serve the API again.",
            "A Vite dev server returns 404 for API routes during local testing.",
        );

        let positive = "The frontend bundle aborts: vite reports ENOENT for public/logo.svg after the alias change.";
        let hits = store
            .recall_for_lanes(positive, 2, MemoryLanes::EXPERIENCE_ONLY)
            .unwrap();
        assert_eq!(
            hits[0].0.title, "Vite bundle fails on a missing public asset after an alias change",
            "the same bundle defect did not rank first: {hits:?}"
        );

        let negative =
            "The vite dev server ignores the proxy setting for /api during local testing.";
        let hits = store
            .recall_for_lanes(negative, 2, MemoryLanes::EXPERIENCE_ONLY)
            .unwrap();
        assert_eq!(
            hits[0].0.title, "Vite dev server ignores the proxy setting",
            "a different vite problem ranked first: {hits:?}"
        );
    }

    /// A data-format name can be the only term connecting a task to the lesson
    /// that explains it. Retrieval must keep `yaml`, `toml` and `json` as
    /// material; a later precision pass must not "fix" recall precision by
    /// adding them to a stopword list.
    #[test]
    fn technical_format_names_stay_retrieval_material() {
        let dir = tempfile::tempdir().unwrap();
        let store = MemoryStore::open(dir.path()).unwrap();
        learn_experience(
            &store,
            "experience_yaml_tab_indentation",
            "YAML rejects tab indentation",
            "A `config.yaml` parse failure names the tab position; replacing the tab with spaces made the parse succeed.",
            "A YAML document fails to parse because of tab indentation.",
        );
        learn_experience(
            &store,
            "experience_toml_duplicate_key",
            "TOML rejects a duplicate key",
            "A `Cargo.toml` duplicate-key error names the second definition; removing the duplicate made the manifest load.",
            "A TOML manifest fails to load because of a duplicate key.",
        );
        learn_experience(
            &store,
            "experience_json_trailing_comma",
            "JSON rejects a trailing comma",
            "A JSON payload with a trailing comma is rejected by the parser; removing the comma made the payload valid.",
            "A JSON payload is rejected because of a trailing comma.",
        );

        for (query, expected) in [
            (
                "这个 yaml 配置解析不了，报的是缩进问题",
                "YAML rejects tab indentation",
            ),
            ("toml 清单重复 key 载入失败", "TOML rejects a duplicate key"),
            ("json 载荷多了一个逗号", "JSON rejects a trailing comma"),
        ] {
            let hits = store
                .recall_for_lanes(query, 3, MemoryLanes::EXPERIENCE_ONLY)
                .unwrap();
            assert_eq!(
                hits.first().map(|(entry, _)| entry.title.as_str()),
                Some(expected),
                "`{query}` lost its format lesson: {hits:?}"
            );
        }
    }
}
