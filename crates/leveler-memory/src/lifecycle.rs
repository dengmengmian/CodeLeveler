//! Durable-memory lifecycle: identity, authority, decision and mutation.
//!
//! This module is the single owner of "what should happen to memory when a new
//! statement arrives". Storage mutation lives on [`MemoryStore`] (in `lib.rs`);
//! the *decision* lives here so no UI, executor or tool re-invents the rules.
//!
//! The split is deliberate:
//!
//! ```text
//! parse_durable_fact / candidate
//!         ↓
//! decide(store, candidate)   → MemoryOperation   (pure-ish: reads the store)
//!         ↓
//! commit_candidate(...)      → CommitOutcome     (mutates the store)
//! ```
//!
//! A model never writes the store directly. `remember`/`forget` tools keep
//! their K36 human-approval gate, and ordinary durable-looking user statements
//! remain pending candidates. Only an explicit direct memory command, or an
//! accepted candidate, authorizes an active-memory write.

use std::collections::{HashMap, HashSet};
use std::fs::OpenOptions;

use serde::{Deserialize, Serialize};

use crate::candidates::{CandidateKind, CandidateSource, MemoryCandidate, looks_like_secret};
use crate::{
    MemoryEntry, MemoryError, MemoryKind, MemoryStatus, MemoryStore, entry_from_candidate,
    now_rfc3339, slugify,
};

/// Where a fact came from, and therefore how much weight it carries.
///
/// Ordered weakest to strongest by [`Self::rank`]. A low-authority statement
/// never silently replaces a higher-authority one: the conflict is surfaced
/// instead of resolved.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MemoryAuthority {
    /// The model's own guess. Never overwrites anything by itself.
    #[default]
    ModelInference,
    /// A fact read out of tool output (command, file read).
    ToolObservation,
    /// A fact the runtime observed about itself (session, environment).
    RuntimeObservation,
    /// A fact the project's own sources state (config, code, AGENTS.md).
    ProjectSource,
    /// The user stated it in so many words. The strongest.
    ExplicitUser,
}

impl MemoryAuthority {
    pub fn rank(self) -> u8 {
        match self {
            Self::ModelInference => 0,
            Self::ToolObservation => 1,
            Self::RuntimeObservation => 2,
            Self::ProjectSource => 3,
            Self::ExplicitUser => 4,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::ModelInference => "model_inference",
            Self::ToolObservation => "tool_observation",
            Self::RuntimeObservation => "runtime_observation",
            Self::ProjectSource => "project_source",
            Self::ExplicitUser => "explicit_user",
        }
    }
}

/// How provenance is stored on an active entry.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Provenance {
    pub authority: MemoryAuthority,
    /// Where it came from (`explicit_user_turn`, `agent_remember_tool`, …).
    pub source: String,
    /// Optional short evidence reference. Never a secret.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub evidence: Option<String>,
}

/// Whether a candidate states a fresh fact or corrects an existing one.
///
/// Only a hint: the commit decision re-derives the operation from the store, so
/// a mislabeled candidate cannot overwrite the wrong memory.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CandidateOperation {
    #[default]
    Create,
    Update,
}

/// The operation the decision layer chose. Separated from mutation so it can be
/// asserted on its own.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MemoryOperation {
    /// Nothing with this semantic identity exists yet.
    Create,
    /// An active memory with the same identity must become historical.
    Supersede { existing_id: String },
    /// The candidate is a strict superset of an existing entry with the same
    /// identity: fold the old one into the new current entry. Only fires when
    /// every existing line is already a line of the candidate, so no two
    /// conflicting values are ever concatenated.
    Merge { existing_id: String },
    /// Deliberately not written (duplicate, or not durable).
    Skip { reason: String },
    /// A conflict only a person may settle (insufficient authority).
    NeedsApproval { reason: String },
}

/// The decision plus the entries it was made against, for tests and callers
/// that must show the user what it conflicted with.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MemoryDecision {
    pub operation: MemoryOperation,
    pub related: Vec<MemoryEntry>,
}

/// What `commit_candidate` actually did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AppliedOperation {
    Created,
    Superseded,
    Merged,
    Skipped,
    /// The decision needs a person. Nothing became current; the caller that
    /// owns consent is responsible for parking the candidate.
    NeedsApproval,
}

/// Result of applying a decision. `entry` is the new current memory (if any);
/// `superseded` are the entries moved to history.
#[derive(Debug, Clone)]
pub struct CommitOutcome {
    pub operation: AppliedOperation,
    pub entry: Option<MemoryEntry>,
    pub superseded: Vec<MemoryEntry>,
    pub skipped_reason: Option<String>,
}

/// The kind of lifecycle change, for the runtime/UI event stream.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MemoryLifecycleOp {
    Created,
    Updated,
    Superseded,
    Expired,
    Merged,
}

impl MemoryLifecycleOp {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Created => "created",
            Self::Updated => "updated",
            Self::Superseded => "superseded",
            Self::Expired => "expired",
            Self::Merged => "merged",
        }
    }
}

// ── semantic identity ──────────────────────────────────────────────────────

/// Normalize a subject into a stable identity. Deterministic, not semantic: it
/// is a function of the characters alone, so the same subject always maps to
/// the same key and two different subjects do not collide by construction.
pub fn semantic_key_of(subject: &str) -> String {
    let mut normalized: String = subject
        .chars()
        .filter(|c| c.is_alphanumeric())
        .flat_map(|c| c.to_lowercase())
        .collect();
    // Strip a small set of trailing nouns so "默认模型", "默认模型设置" and
    // "默认" are one fact. Repeat until stable: stripping "设置" can expose
    // "模型", and a single ordered pass would leave the two spellings with
    // different identities.
    loop {
        let mut stripped = false;
        for suffix in ["模型", "设置", "选项", "名称", "model", "value", "setting"] {
            if normalized.len() > suffix.len() && normalized.ends_with(suffix) {
                normalized.truncate(normalized.len() - suffix.len());
                stripped = true;
            }
        }
        if !stripped {
            break;
        }
    }
    if normalized.is_empty() {
        "fact".to_string()
    } else if normalized.is_ascii() && normalized.len() <= 48 {
        format!("k-{}", slugify(&normalized))
    } else {
        // Long subjects (and all CJK) use a hash. `slugify` truncates ids for
        // readability, and truncating an identity would merge two facts that
        // merely share a prefix.
        format!("k-{}", crate::short_hash(&normalized))
    }
}

/// Semantic identity for both current entries and legacy direct writes that
/// predate the `key` field. The latter can be upgraded from their own durable
/// assignment text without guessing from unrelated prose.
pub(crate) fn durable_entry_key(entry: &MemoryEntry) -> Option<String> {
    entry.key.clone().or_else(|| {
        parse_durable_fact(&entry.body)
            .and_then(|candidate| candidate.semantic_key.or(candidate.key))
    })
}

// ── explicit durable-fact parsing ──────────────────────────────────────────

const DURABLE_PREFIXES: [&str; 7] = [
    "以后",
    "今后",
    "从现在起",
    "这个项目",
    "本项目",
    "本仓库",
    "这个仓库",
];

const STRONG_MARKERS: [&str; 5] = ["固定为", "固定是", "不再", "禁止", "必须"];

/// Separators that state an assignment: `subject <sep> value`. Ordered by scan
/// position, not preference, so the earliest one wins.
const SEPARATORS: [&str; 12] = [
    "固定为",
    "固定是",
    "变更为",
    "设置为",
    "设为",
    "改成",
    "改为",
    "默认使用",
    "默认用",
    "统一使用",
    "统一用",
    "=",
];

const CORRECTION_MARKERS: [&str; 7] =
    ["改成", "改为", "变更", "不再", "之前那个", "原来的", "修正"];

/// Parse a statement that the user explicitly framed as a lasting project rule.
///
/// Deliberately narrow. It fires only on an explicit durability marker plus an
/// assignment shape, and refuses secrets, code, quotes and any body long enough
/// to be a task rather than a fact. A miss is recoverable (the soft-preference
/// path still proposes a candidate); a false positive rewrites long-term state.
pub fn parse_durable_fact(text: &str) -> Option<MemoryCandidate> {
    let t = text.trim();
    if t.is_empty() || t.chars().count() > 300 {
        return None;
    }
    if looks_like_secret(t) || t.contains('`') {
        return None;
    }
    if t.contains('“') || t.contains('”') || t.contains('"') {
        return None;
    }
    if !has_durability_marker(t) {
        return None;
    }
    let (subject_raw, sep, value_raw) = split_fact(t)?;
    let value = value_raw
        .trim()
        .trim_end_matches(['。', '.', '！', '!', '；', ';'])
        .trim();
    if value.is_empty() || value.chars().count() > 160 {
        return None;
    }
    let subject = match clean_subject(subject_raw) {
        Some(s) => s,
        None if sep.starts_with("默认") => "默认".to_string(),
        None if sep.starts_with("统一") => "统一".to_string(),
        None => return None,
    };
    if subject.chars().count() > 60 {
        return None;
    }
    let key = semantic_key_of(&subject);
    let title = format!("{}：{}", subject, first_line(value));
    let body = format!("{subject}：{value}");
    let mut candidate = MemoryCandidate::new(
        title,
        body,
        CandidateKind::Free,
        Some(key.clone()),
        CandidateSource::UserExplicit,
        vec![
            "explicit".to_string(),
            "decision".to_string(),
            format!("subject:{subject}"),
        ],
    )
    .ok()?;
    candidate.semantic_key = Some(key);
    candidate.authority = MemoryAuthority::ExplicitUser;
    candidate.operation = if is_correction(t) {
        CandidateOperation::Update
    } else {
        CandidateOperation::Create
    };
    Some(candidate)
}

fn has_durability_marker(text: &str) -> bool {
    if DURABLE_PREFIXES.iter().any(|p| text.starts_with(p)) {
        return true;
    }
    STRONG_MARKERS.iter().any(|m| text.contains(m))
}

fn split_fact(text: &str) -> Option<(&str, &'static str, &str)> {
    let mut best: Option<(usize, &'static str)> = None;
    for sep in SEPARATORS {
        if let Some(index) = text.find(sep)
            && best.map(|(b, _)| index < b).unwrap_or(true)
        {
            best = Some((index, sep));
        }
    }
    let (index, sep) = best?;
    Some((&text[..index], sep, &text[index + sep.len()..]))
}

fn clean_subject(raw: &str) -> Option<String> {
    let mut s = raw.trim();
    const STRIP: [&str; 20] = [
        "从现在起",
        "这个项目的",
        "本项目的",
        "这个仓库的",
        "本仓库的",
        "这个项目",
        "本项目",
        "这个仓库",
        "本仓库",
        "以后",
        "今后",
        "项目里",
        "仓库里",
        "里",
        "中",
        "的",
        "：",
        ":",
        "，",
        ",",
    ];
    loop {
        let before = s;
        for prefix in STRIP {
            if let Some(rest) = s.strip_prefix(prefix) {
                s = rest.trim_start();
            }
        }
        if s == before {
            break;
        }
    }
    let s = s
        .trim_end_matches(['的', '：', ':', '，', ',', '。'])
        .trim();
    if s.is_empty() {
        None
    } else {
        Some(s.to_string())
    }
}

fn is_correction(text: &str) -> bool {
    CORRECTION_MARKERS.iter().any(|m| text.contains(m))
}

fn first_line(value: &str) -> String {
    let line = value.lines().next().unwrap_or(value).trim();
    line.chars().take(48).collect()
}

/// Build the candidate behind the model's `remember` tool.
///
/// The model must name a stable subject so the lifecycle can tell "this
/// replaces an earlier value" from "this is a new fact". The subject is
/// normalized by the SAME rules as every other path; the model never supplies
/// a raw key. The candidate carries model-inference authority regardless of
/// which permission profile approved the tool call.
pub fn agent_candidate(
    title: &str,
    body: &str,
    kind: MemoryKind,
    tags: Vec<String>,
    subject: &str,
    evidence: Option<String>,
) -> Result<MemoryCandidate, MemoryError> {
    let subject = crate::canonical_subject(subject);
    let subject = subject.trim();
    if subject.is_empty() {
        return Err(MemoryError::Invalid(
            "remember requires a non-empty subject".to_string(),
        ));
    }
    if subject.chars().count() > 96 {
        return Err(MemoryError::Invalid(
            "remember subject is too long".to_string(),
        ));
    }
    if title.trim().is_empty() || title.chars().count() > 200 {
        return Err(MemoryError::Invalid(
            "remember title must be 1..=200 characters".to_string(),
        ));
    }
    if body.trim().is_empty() || body.chars().count() > 8_000 {
        return Err(MemoryError::Invalid(
            "remember body must be 1..=8000 characters".to_string(),
        ));
    }
    if evidence
        .as_deref()
        .is_some_and(|evidence| evidence.chars().count() > 500)
    {
        return Err(MemoryError::Invalid(
            "remember evidence is too long".to_string(),
        ));
    }
    if tags.len() > 16 || tags.iter().any(|tag| tag.chars().count() > 100) {
        return Err(MemoryError::Invalid(
            "remember tags are too many or too long".to_string(),
        ));
    }
    let key = semantic_key_of(subject);
    let mut tags = tags;
    tags.retain(|tag| {
        let tag = tag.trim_start();
        !tag.starts_with("kind:") && !tag.starts_with("subject:")
    });
    tags.push(format!("kind:{}", kind.as_str()));
    tags.push(format!("subject:{subject}"));
    let mut candidate = MemoryCandidate::new(
        title,
        body,
        kind.candidate_kind(),
        Some(key.clone()),
        CandidateSource::AgentProposed,
        tags,
    )?;
    candidate.semantic_key = Some(key);
    candidate.authority = MemoryAuthority::ModelInference;
    candidate.evidence = evidence;
    Ok(candidate)
}

pub(crate) fn candidate_subject(candidate: &MemoryCandidate) -> Option<&str> {
    candidate
        .tags
        .iter()
        .find_map(|tag| tag.strip_prefix("subject:"))
        .filter(|subject| !subject.is_empty())
}

pub(crate) fn entry_matches_candidate(entry: &MemoryEntry, candidate: &MemoryCandidate) -> bool {
    let identity = candidate
        .semantic_key
        .as_deref()
        .or(candidate.key.as_deref());
    if identity.is_some() && durable_entry_key(entry).as_deref() == identity {
        return true;
    }
    if entry.key.is_some() {
        return false;
    }
    if MemoryKind::of(entry) != MemoryKind::Preference
        || entry.provenance.as_ref().map(|value| value.source.as_str()) != Some("user_direct")
    {
        return false;
    }
    let Some(subject) = candidate_subject(candidate) else {
        return false;
    };
    let normalized_subject: String = subject
        .chars()
        .filter(|character| character.is_alphanumeric())
        .flat_map(|character| character.to_lowercase())
        .collect();
    if normalized_subject.chars().count() < 4 {
        return false;
    }
    let mut body = entry.body.trim();
    for prefix in ["从现在起", "以后", "今后"] {
        if let Some(rest) = body.strip_prefix(prefix) {
            body = rest.trim_start();
            break;
        }
    }
    for prefix in ["不要", "不再", "别"] {
        if let Some(rest) = body.strip_prefix(prefix) {
            body = rest.trim_start();
            break;
        }
    }
    let body = body
        .trim_end_matches(['。', '.', '！', '!', '；', ';'])
        .trim_end_matches('了')
        .trim();
    let normalized_body: String = body
        .chars()
        .filter(|character| character.is_alphanumeric())
        .flat_map(|character| character.to_lowercase())
        .collect();
    normalized_body == normalized_subject
}

// ── decision ───────────────────────────────────────────────────────────────

fn source_label(source: CandidateSource) -> &'static str {
    match source {
        CandidateSource::UserExplicit => "explicit_user_turn",
        CandidateSource::SystemPropose => "system_propose",
        CandidateSource::SystemInferred => "system_inferred",
        CandidateSource::AgentProposed => "agent_remember_tool",
        CandidateSource::UserRestore => "user_restore",
        CandidateSource::UserAccepted => "user_accepted",
    }
}

/// Decide what a candidate should do to the store. Reads the store; mutates
/// nothing.
pub fn decide(
    store: &MemoryStore,
    candidate: &MemoryCandidate,
    now: &str,
) -> Result<MemoryDecision, MemoryError> {
    let active: Vec<MemoryEntry> = store
        .list_active()?
        .into_iter()
        .filter(|e| e.effective_status() == MemoryStatus::Active && !e.is_expired_at(now))
        .collect();

    // Idempotent: an identical fact already active is not a second memory.
    if let Some(existing) = active.iter().find(|e| {
        e.title.trim() == candidate.title.trim() && e.body.trim() == candidate.body.trim()
    }) {
        return Ok(MemoryDecision {
            operation: MemoryOperation::Skip {
                reason: format!("identical to active memory {}", existing.id),
            },
            related: vec![existing.clone()],
        });
    }

    let identity = candidate
        .semantic_key
        .as_deref()
        .or(candidate.key.as_deref());
    if identity.is_some() {
        let mut related: Vec<MemoryEntry> = active
            .into_iter()
            .filter(|entry| entry_matches_candidate(entry, candidate))
            .collect();
        related.sort_by(|a, b| b.updated_at.cmp(&a.updated_at));
        if let Some(existing) = related.first() {
            // Same identity and same stated value: nothing new, whatever the
            // title says.
            if existing.body.trim() == candidate.body.trim() {
                return Ok(MemoryDecision {
                    operation: MemoryOperation::Skip {
                        reason: format!("existing memory {} already states this", existing.id),
                    },
                    related,
                });
            }
            // A delayed retry of a value this identity already replaced must
            // not rewind the chain. The retry is recognizable structurally:
            // the candidate's value is an ancestor of the current entry.
            // Timestamps cannot carry this (stored at second precision), and
            // the chain is the authoritative history.
            if store.candidate_retries_history(candidate, &existing.id)? {
                return Ok(MemoryDecision {
                    operation: MemoryOperation::Skip {
                        reason: format!(
                            "candidate was already superseded by {}",
                            existing.id
                        ),
                    },
                    related,
                });
            }
            let incoming = candidate.authority.rank();
            let held = existing.authority().rank();
            // Deterministic merge: only when the existing entry's lines are all
            // present in the candidate (a strict superset). Two entries with
            // different values share no such relation and are never joined.
            let candidate_lines = body_lines(&candidate.body);
            let existing_lines = body_lines(&existing.body);
            if existing_lines.is_subset(&candidate_lines)
                && existing_lines != candidate_lines
                && incoming >= held
            {
                return Ok(MemoryDecision {
                    operation: MemoryOperation::Merge {
                        existing_id: existing.id.clone(),
                    },
                    related,
                });
            }
            if candidate_lines.is_subset(&existing_lines) && candidate_lines != existing_lines {
                return Ok(MemoryDecision {
                    operation: MemoryOperation::Skip {
                        reason: format!("existing memory {} already covers this", existing.id),
                    },
                    related,
                });
            }
            if incoming > held {
                return Ok(MemoryDecision {
                    operation: MemoryOperation::Supersede {
                        existing_id: existing.id.clone(),
                    },
                    related,
                });
            }
            // Same class: the user's own correction, or a user-commanded
            // restore, overwrites.
            if incoming == held
                && matches!(
                    candidate.source,
                    CandidateSource::UserExplicit
                        | CandidateSource::UserRestore
                        | CandidateSource::UserAccepted
                )
            {
                return Ok(MemoryDecision {
                    operation: MemoryOperation::Supersede {
                        existing_id: existing.id.clone(),
                    },
                    related,
                });
            }
            // The model may correct its OWN earlier low-authority record when
            // it supplies new evidence. The new entry keeps model authority;
            // approval to run the tool never upgrades it to a user fact.
            if incoming == held
                && candidate.source == CandidateSource::AgentProposed
                && existing.provenance.as_ref().map(|value| value.source.as_str())
                    == Some("agent_remember_tool")
            {
                if candidate
                    .evidence
                    .as_deref()
                    .is_some_and(|evidence| !evidence.trim().is_empty())
                {
                    return Ok(MemoryDecision {
                        operation: MemoryOperation::Supersede {
                            existing_id: existing.id.clone(),
                        },
                        related,
                    });
                }
                return Ok(MemoryDecision {
                    operation: MemoryOperation::NeedsApproval {
                        reason: "an agent update needs new evidence and a person".to_string(),
                    },
                    related,
                });
            }
            return Ok(MemoryDecision {
                operation: MemoryOperation::NeedsApproval {
                    reason: format!(
                        "{} cannot supersede {} without a person",
                        candidate.authority.as_str(),
                        existing.authority().as_str()
                    ),
                },
                related,
            });
        }
        return Ok(MemoryDecision {
            operation: MemoryOperation::Create,
            related,
        });
    }

    // No semantic identity: only an explicit user statement may auto-create.
    if candidate.authority.rank() < MemoryAuthority::ExplicitUser.rank() {
        return Ok(MemoryDecision {
            operation: MemoryOperation::NeedsApproval {
                reason: "candidate has no semantic identity and is below explicit-user authority"
                    .to_string(),
            },
            related: Vec::new(),
        });
    }
    Ok(MemoryDecision {
        operation: MemoryOperation::Create,
        related: Vec::new(),
    })
}

impl MemoryStore {
    pub(crate) fn acquire_lifecycle_lock(&self) -> Result<std::fs::File, MemoryError> {
        let lock = OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .truncate(false)
            .open(self.root().join(".lifecycle.lock"))?;
        fs2::FileExt::lock_exclusive(&lock)?;
        Ok(lock)
    }

    /// Decide and apply. The autonomous path: caller has already established
    /// that the candidate is the user's own statement (or otherwise authorized).
    ///
    /// Ordering on supersede is **new-first**: the new truth is made durable
    /// before the old entry is moved to history. A crash between the two leaves
    /// two active entries, never zero; recall deduplicates by semantic key, so
    /// the extra copy is invisible.
    pub fn commit_candidate(
        &self,
        candidate: &MemoryCandidate,
    ) -> Result<CommitOutcome, MemoryError> {
        // Decision and apply are one store transaction. The data itself stays
        // file-backed, but the advisory lock serializes every autonomous
        // writer using this project root, including writers in other
        // processes. Keeping the file handle alive holds the lock until the
        // outcome has been fully applied.
        crate::candidates::validate_candidate(candidate)?;
        let _lifecycle_lock = self.acquire_lifecycle_lock()?;
        self.apply_candidate_locked(candidate, &now_rfc3339())
    }

    /// The decision and mutation half of a commit. The caller holds the
    /// project-wide lifecycle lock, so every path that has already taken it
    /// (a user write, a restore, a candidate commit) shares one transaction.
    pub(crate) fn apply_candidate_locked(
        &self,
        candidate: &MemoryCandidate,
        now: &str,
    ) -> Result<CommitOutcome, MemoryError> {
        // New-first replacement guarantees no gap in current truth, so a
        // crash can leave two physical active files. Heal that durable residue
        // before deriving a fresh decision; an at-least-once retry then sees
        // the already-written replacement and becomes an idempotent skip.
        self.reconcile_active_truths(now)?;
        let decision = decide(self, candidate, now)?;
        let outcome: Result<CommitOutcome, MemoryError> = match decision.operation {
            MemoryOperation::Skip { reason } => Ok(CommitOutcome {
                operation: AppliedOperation::Skipped,
                entry: None,
                superseded: Vec::new(),
                skipped_reason: Some(reason),
            }),
            MemoryOperation::NeedsApproval { reason } => Ok(CommitOutcome {
                operation: AppliedOperation::NeedsApproval,
                entry: None,
                superseded: Vec::new(),
                skipped_reason: Some(reason),
            }),
            MemoryOperation::Create => {
                let entry = self.write_candidate(candidate, None)?;
                Ok(CommitOutcome {
                    operation: AppliedOperation::Created,
                    entry: Some(entry),
                    superseded: Vec::new(),
                    skipped_reason: None,
                })
            }
            MemoryOperation::Supersede { existing_id } => {
                let old = self.read_active(&existing_id)?;
                let new = self.write_candidate(candidate, Some(&old))?;
                let archived = self.mark_superseded_unlocked(&old, &new.id, now)?;
                Ok(CommitOutcome {
                    operation: AppliedOperation::Superseded,
                    entry: Some(new),
                    superseded: vec![archived],
                    skipped_reason: None,
                })
            }
            MemoryOperation::Merge { existing_id } => {
                let old = self.read_active(&existing_id)?;
                let new = self.write_candidate(candidate, Some(&old))?;
                let archived = self.mark_superseded_unlocked(&old, &new.id, now)?;
                Ok(CommitOutcome {
                    operation: AppliedOperation::Merged,
                    entry: Some(new),
                    superseded: vec![archived],
                    skipped_reason: None,
                })
            }
        };
        let outcome = outcome?;
        if matches!(
            outcome.operation,
            AppliedOperation::Created | AppliedOperation::Superseded | AppliedOperation::Merged
        ) && let Some(key) = candidate
            .semantic_key
            .as_deref()
            .or(candidate.key.as_deref())
        {
            self.clear_pending_for_key(key)?;
        }
        Ok(outcome)
    }

    /// Whether this candidate's value is already an ancestor of the current
    /// entry. Used to recognize a delayed retry: the same candidate applied
    /// once, then replaced by a newer value. A retry must be an idempotent
    /// skip, never a rewind.
    fn candidate_retries_history(
        &self,
        candidate: &MemoryCandidate,
        current_id: &str,
    ) -> Result<bool, MemoryError> {
        let mut next = self.read_active(current_id)?.supersedes;
        let mut seen = HashSet::new();
        while let Some(id) = next {
            if !seen.insert(id.clone()) {
                // Malformed cycle in legacy residue: stop rather than loop.
                return Ok(false);
            }
            let archived = match self.read_archived(&id) {
                Ok(entry) => entry,
                Err(MemoryError::NotFound(_)) => return Ok(false),
                Err(error) => return Err(error),
            };
            if archived.body.trim() == candidate.body.trim() {
                // Only a candidate that existed BEFORE the ancestor was
                // replaced can be a delayed retry. A fresh statement of the
                // same value later is the user changing their mind back and
                // must be allowed to win.
                return Ok(archived
                    .superseded_at
                    .as_deref()
                    .map_or(true, |at| candidate.created_at.as_str() < at));
            }
            next = archived.supersedes.clone();
        }
        Ok(false)
    }

    fn read_archived(&self, id: &str) -> Result<MemoryEntry, MemoryError> {
        let path = self.archive_path(id);
        if !path.exists() {
            return Err(MemoryError::NotFound(id.to_string()));
        }
        let raw = std::fs::read_to_string(path)?;
        Ok(serde_json::from_str(&raw)?)
    }

    /// Restore the physical invariant of at most one current entry per
    /// semantic identity.
    ///
    /// This is apply recovery, not a second decision engine. Multiple active
    /// files with one key can only be residue from the new-first supersede
    /// protocol (including an older concurrent writer). The successor link is
    /// authoritative when present; authority and durable timestamps provide a
    /// deterministic fallback for forked legacy residue.
    fn reconcile_active_truths(&self, now: &str) -> Result<(), MemoryError> {
        let mut by_key: HashMap<String, Vec<MemoryEntry>> = HashMap::new();
        for entry in self.list_active()? {
            if entry.effective_status() != MemoryStatus::Active || entry.is_expired_at(now) {
                continue;
            }
            if let Some(key) = durable_entry_key(&entry) {
                by_key.entry(key).or_default().push(entry);
            }
        }

        for entries in by_key.into_values().filter(|entries| entries.len() > 1) {
            let winner = current_truth_of(&entries)
                .expect("group has at least two entries")
                .clone();
            for old in entries.into_iter().filter(|entry| entry.id != winner.id) {
                self.mark_superseded_unlocked(&old, &winner.id, now)?;
            }
        }
        Ok(())
    }

    /// Promote a candidate to an active entry, carrying provenance forward.
    fn write_candidate(
        &self,
        candidate: &MemoryCandidate,
        superseding: Option<&MemoryEntry>,
    ) -> Result<MemoryEntry, MemoryError> {
        let mut entry = entry_from_candidate(candidate);
        entry.provenance = Some(Provenance {
            authority: candidate.authority,
            source: source_label(candidate.source).to_string(),
            evidence: candidate.evidence.clone(),
        });
        entry.expires_at = candidate.expires_at.clone();
        entry.status = MemoryStatus::Active;
        if let Some(old) = superseding {
            entry.supersedes = Some(old.id.clone());
        }
        self.remember_deduplicated_unlocked(entry)
    }

    /// Move one active entry to history as superseded. Idempotent-safe: it
    /// writes the archive copy before removing the active one.
    pub fn mark_superseded(
        &self,
        old: &MemoryEntry,
        new_id: &str,
        now: &str,
    ) -> Result<MemoryEntry, MemoryError> {
        let _lifecycle_lock = self.acquire_lifecycle_lock()?;
        self.mark_superseded_unlocked(old, new_id, now)
    }

    fn mark_superseded_unlocked(
        &self,
        old: &MemoryEntry,
        new_id: &str,
        now: &str,
    ) -> Result<MemoryEntry, MemoryError> {
        let mut archived = old.clone();
        archived.status = MemoryStatus::Superseded;
        archived.superseded_at = Some(now.to_string());
        archived.superseded_by = Some(new_id.to_string());
        archived.archived_at = Some(now.to_string());
        // Supersede is a retention boundary like forget/expiry: the historical
        // copy stays for audit, but a credential must not be written into it.
        if crate::is_sensitive(&archived) {
            crate::redact_entry_for_archive(&mut archived);
        }
        let json = serde_json::to_string_pretty(&archived)?;
        crate::write_atomically_pub(&self.archive_path(&archived.id), json.as_bytes())?;
        match std::fs::remove_file(self.active_path(&archived.id)) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(MemoryError::Io(error)),
        }
        Ok(archived)
    }

    /// Restore a historical entry as the current truth.
    ///
    /// This is a user command, so it carries explicit-user authority and runs
    /// through the lifecycle: a current value for the same identity is
    /// superseded, never left active beside the restored copy. History is
    /// preserved; the restored value becomes a NEW entry whose provenance says
    /// where it came from. A credential-bearing history record was redacted at
    /// the retention boundary and is refused rather than restored as redacted
    /// text.
    pub fn restore(&self, id: &str) -> Result<MemoryEntry, MemoryError> {
        let _lifecycle_lock = self.acquire_lifecycle_lock()?;
        if self.active_path(id).exists() {
            return self.read_active(id);
        }
        let archived = self.read_archived(id)?;
        // The retention boundary redacts credential-bearing records. Restoring
        // one would either resurrect a secret or silently restore placeholder
        // text, so both are refused.
        if crate::is_sensitive(&archived)
            || archived.title == crate::REDACTED_TITLE
            || archived.body == crate::REDACTED_BODY
        {
            return Err(MemoryError::Invalid(
                "sensitive history was redacted and cannot be restored".to_string(),
            ));
        }
        // Ephemeral state must never become lasting truth. Restoring a
        // time-bound record as a project fact would do exactly that.
        if archived.scope == crate::MemoryScope::Ephemeral || archived.expires_at.is_some() {
            return Err(MemoryError::Invalid(
                "time-bound history cannot be restored as lasting memory".to_string(),
            ));
        }
        let key = durable_entry_key(&archived);
        let kind = match crate::MemoryKind::of(&archived) {
            crate::MemoryKind::Preference => crate::candidates::CandidateKind::Preference,
            _ => crate::candidates::CandidateKind::Free,
        };
        let mut tags = archived.tags.clone();
        if let Some(declared) = archived.kind.as_deref() {
            tags.push(format!("kind:{declared}"));
        }
        let mut candidate = MemoryCandidate::new(
            archived.title.clone(),
            archived.body.clone(),
            kind,
            key.clone(),
            crate::candidates::CandidateSource::UserRestore,
            tags,
        )?;
        candidate.authority = MemoryAuthority::ExplicitUser;
        candidate.evidence = Some(format!("restored from memory {}", archived.id));
        let outcome = self.apply_candidate_locked(&candidate, &now_rfc3339())?;
        match outcome.operation {
            AppliedOperation::Created
            | AppliedOperation::Superseded
            | AppliedOperation::Merged => outcome.entry.ok_or_else(|| {
                MemoryError::Invalid("restore produced no current entry".to_string())
            }),
            AppliedOperation::Skipped => {
                let current = self.effective_active()?.into_iter().find(|entry| {
                    key.as_deref().is_some_and(|key| {
                        durable_entry_key(entry).as_deref() == Some(key)
                    })
                });
                current.ok_or_else(|| {
                    MemoryError::Invalid(
                        outcome
                            .skipped_reason
                            .unwrap_or_else(|| "nothing to restore".to_string()),
                    )
                })
            }
            AppliedOperation::NeedsApproval => Err(MemoryError::Invalid(
                outcome
                    .skipped_reason
                    .unwrap_or_else(|| "restore needs a person".to_string()),
            )),
        }
    }

    /// Archive every active entry whose `expires_at` has passed.
    ///
    /// Runs at the start of a turn; a failure to archive is not fatal — recall
    /// filters expired entries regardless, so the store staying slightly stale
    /// never leaks an expired fact into context.
    pub fn expire_due(&self) -> Result<Vec<MemoryEntry>, MemoryError> {
        let _lifecycle_lock = self.acquire_lifecycle_lock()?;
        let now = now_rfc3339();
        let mut out = Vec::new();
        for entry in self.list_active()? {
            if entry.effective_status() != MemoryStatus::Active || !entry.is_expired_at(&now) {
                continue;
            }
            let mut archived = entry.clone();
            archived.status = MemoryStatus::Expired;
            archived.archived_at = Some(now.clone());
            if crate::is_sensitive(&archived) {
                crate::redact_entry_for_archive(&mut archived);
            }
            let json = serde_json::to_string_pretty(&archived)?;
            crate::write_atomically_pub(&self.archive_path(&archived.id), json.as_bytes())?;
            let _ = std::fs::remove_file(self.active_path(&archived.id));
            out.push(archived);
        }
        Ok(out)
    }

    /// Active entries after lifecycle filtering: lifecycle-active, not expired,
    /// deduplicated by semantic key (best authority, then newest).
    ///
    /// This is the ONE read behind recall, standing preferences and the catalog,
    /// so a superseded/expired copy can never leak into context through a
    /// different path.
    pub fn effective_active(&self) -> Result<Vec<MemoryEntry>, MemoryError> {
        let now = now_rfc3339();
        let entries: Vec<MemoryEntry> = self
            .list_active()?
            .into_iter()
            .filter(|entry| {
                entry.effective_status() == MemoryStatus::Active && !entry.is_expired_at(&now)
            })
            .collect();

        let mut groups: HashMap<String, Vec<MemoryEntry>> = HashMap::new();
        for entry in &entries {
            if let Some(key) = durable_entry_key(entry) {
                groups.entry(key).or_default().push(entry.clone());
            }
        }
        let winners: HashMap<String, MemoryEntry> = groups
            .into_iter()
            .filter_map(|(key, group)| {
                current_truth_of(&group).map(|winner| (key, winner.clone()))
            })
            .collect();

        // Preserve the store's stable title order, but emit only the terminal
        // entry of each identity. A crash can leave the replacement and the
        // old copy active together; the supersedes link decides which is real.
        let mut out = Vec::with_capacity(entries.len());
        let mut emitted = HashSet::new();
        for entry in entries {
            match durable_entry_key(&entry) {
                Some(key) => {
                    if !emitted.insert(key.clone()) {
                        continue;
                    }
                    if let Some(winner) = winners.get(&key) {
                        out.push(winner.clone());
                    }
                }
                None => out.push(entry),
            }
        }
        Ok(out)
    }
}

/// Non-empty, trimmed, case-folded lines of a body, as a set. Used only by the
/// deterministic merge check, where a superset means "strictly more complete".
fn body_lines(body: &str) -> std::collections::BTreeSet<String> {
    body.lines()
        .map(|line| line.trim().to_lowercase())
        .filter(|line| !line.is_empty())
        .collect()
}

/// The one entry that is current among active copies of one identity.
///
/// The supersedes links are authoritative: a terminal entry is not named as
/// the predecessor of another active entry, so following them survives cycles
/// of crash residue and A -> B -> C chains alike. Authority and timestamps are
/// only the deterministic fallback for forked legacy residue, where timestamps
/// have second precision and cannot order two writes.
fn current_truth_of(entries: &[MemoryEntry]) -> Option<&MemoryEntry> {
    if entries.is_empty() {
        return None;
    }
    let superseded_ids: HashSet<&str> = entries
        .iter()
        .filter_map(|entry| entry.supersedes.as_deref())
        .collect();
    let terminal: Vec<&MemoryEntry> = entries
        .iter()
        .filter(|entry| !superseded_ids.contains(entry.id.as_str()))
        .collect();
    let candidates = if terminal.is_empty() {
        // Defensive fallback for malformed cyclic residue.
        entries.iter().collect()
    } else {
        terminal
    };
    candidates
        .into_iter()
        .max_by(|left, right| compare_current_truth(left, right))
}

fn compare_current_truth(left: &MemoryEntry, right: &MemoryEntry) -> std::cmp::Ordering {
    left.authority()
        .rank()
        .cmp(&right.authority().rank())
        .then_with(|| left.updated_at.cmp(&right.updated_at))
        .then_with(|| left.created_at.cmp(&right.created_at))
        .then_with(|| left.id.cmp(&right.id))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn store() -> (tempfile::TempDir, MemoryStore) {
        let dir = tempdir().unwrap();
        let store = MemoryStore::open(dir.path()).unwrap();
        (dir, store)
    }

    /// Suffix stripping must be confluent: "默认模型设置" hides "模型" behind
    /// "设置", and a single ordered pass left two identities for one subject.
    /// Ids truncate for readability; identity must not. Two long subjects that
    /// share their first characters are different facts.
    #[test]
    fn long_subjects_with_the_same_prefix_do_not_collide() {
        let prefix = "release.probe.codename.".to_string() + &"a".repeat(60);
        assert_ne!(
            semantic_key_of(&(prefix.clone() + "alpha")),
            semantic_key_of(&(prefix + "bravo"))
        );
    }

    #[test]
    fn semantic_identity_strips_trailing_nouns_until_stable() {
        let base = semantic_key_of("默认");
        for subject in ["默认模型", "默认模型设置", "默认模型设置选项", "默认设置"] {
            assert_eq!(semantic_key_of(subject), base, "{subject}");
        }
        assert_eq!(
            semantic_key_of("default model setting"),
            semantic_key_of("default")
        );
    }

    #[test]
    fn a_probe_code_name_assignment_is_a_durable_fact() {
        let c = parse_durable_fact("以后这个项目的发布探针代号固定为 ORANGE-7319").expect("fact");
        assert_eq!(c.authority, MemoryAuthority::ExplicitUser);
        assert_eq!(c.source, CandidateSource::UserExplicit);
        assert_eq!(
            c.semantic_key.as_deref(),
            Some(semantic_key_of("发布探针代号").as_str())
        );
        assert!(c.body.contains("ORANGE-7319"));
    }

    #[test]
    fn a_durable_fact_needs_a_marker_and_an_assignment() {
        for text in [
            "帮我看一下这个组件",
            "ORANGE-7319",
            "以后都用 pnpm",
            "我通常希望输出短一点",
        ] {
            assert!(
                parse_durable_fact(text).is_none(),
                "must not be durable: {text}"
            );
        }
    }

    #[test]
    fn a_correction_and_its_original_share_one_key() {
        let a = parse_durable_fact("以后这个项目的发布探针代号固定为 ORANGE-7319").unwrap();
        let b = parse_durable_fact("以后发布探针代号改成 BLUE-4821").unwrap();
        assert_eq!(a.semantic_key, b.semantic_key);
        assert_eq!(b.operation, CandidateOperation::Update);
    }

    #[test]
    fn explicit_create_then_supersede_leaves_one_current_truth() {
        let (_dir, store) = store();
        let first = parse_durable_fact("以后这个项目的发布探针代号固定为 ORANGE-7319").unwrap();
        let created = store.commit_candidate(&first).unwrap();
        assert_eq!(created.operation, AppliedOperation::Created);
        let first_id = created.entry.unwrap().id;
        assert_eq!(store.effective_active().unwrap().len(), 1);

        let second = parse_durable_fact("以后发布探针代号改成 BLUE-4821").unwrap();
        let updated = store.commit_candidate(&second).unwrap();
        assert_eq!(updated.operation, AppliedOperation::Superseded);
        let current = store.effective_active().unwrap();
        assert_eq!(current.len(), 1, "old must not stay current");
        assert!(current[0].body.contains("BLUE-4821"));
        assert_eq!(current[0].supersedes.as_deref(), Some(first_id.as_str()));

        // Historical chain is preserved, not deleted.
        let archived = store.list_archived().unwrap();
        assert_eq!(archived.len(), 1);
        assert_eq!(archived[0].status, MemoryStatus::Superseded);
        assert_eq!(
            archived[0].superseded_by.as_deref(),
            Some(current[0].id.as_str())
        );
        assert!(archived[0].body.contains("ORANGE-7319"));
    }

    #[test]
    fn repeating_the_same_fact_creates_no_duplicate() {
        let (_dir, store) = store();
        let fact = parse_durable_fact("以后这个项目的发布探针代号固定为 ORANGE-7319").unwrap();
        store.commit_candidate(&fact).unwrap();
        let again = store.commit_candidate(&fact).unwrap();
        assert_eq!(again.operation, AppliedOperation::Skipped);
        assert_eq!(store.effective_active().unwrap().len(), 1);
    }

    #[test]
    fn retry_reconciles_crash_after_new_truth_became_durable() {
        let (_dir, store) = store();
        let first = parse_durable_fact("以后这个项目的发布探针代号固定为 ORANGE-7319").unwrap();
        let old = store
            .commit_candidate(&first)
            .unwrap()
            .entry
            .expect("created entry");
        let second = parse_durable_fact("以后发布探针代号改成 BLUE-4821").unwrap();

        // Fault injection: this is the exact durable state left by a process
        // dying after the replacement write and before the old entry is moved
        // to history.
        let replacement = store.write_candidate(&second, Some(&old)).unwrap();
        assert_eq!(store.list_active().unwrap().len(), 2, "injected residue");

        let retry = store.commit_candidate(&second).unwrap();
        assert_eq!(retry.operation, AppliedOperation::Skipped);
        let physical = store.list_active().unwrap();
        assert_eq!(physical.len(), 1, "retry must heal physical active state");
        assert_eq!(physical[0].id, replacement.id);
        let archived = store.list_archived().unwrap();
        assert_eq!(archived.len(), 1);
        assert_eq!(archived[0].id, old.id);
        assert_eq!(
            archived[0].superseded_by.as_deref(),
            Some(replacement.id.as_str())
        );
    }

    #[test]
    fn concurrent_commits_leave_one_physical_current_truth() {
        use std::sync::{Arc, Barrier};

        let (_dir, store) = store();
        let key = semantic_key_of("并发写入模型");
        let mut initial = MemoryCandidate::new(
            "初始模型",
            "并发写入模型：initial",
            CandidateKind::Free,
            Some(key.clone()),
            CandidateSource::UserExplicit,
            vec![],
        )
        .unwrap();
        initial.authority = MemoryAuthority::ExplicitUser;
        store.commit_candidate(&initial).unwrap();

        let writers = 12;
        let barrier = Arc::new(Barrier::new(writers));
        let mut threads = Vec::new();
        for index in 0..writers {
            let store = store.clone();
            let key = key.clone();
            let barrier = Arc::clone(&barrier);
            threads.push(std::thread::spawn(move || {
                let mut candidate = MemoryCandidate::new(
                    format!("并发模型 {index}"),
                    format!("并发写入模型：value-{index}"),
                    CandidateKind::Free,
                    Some(key),
                    CandidateSource::UserExplicit,
                    vec![],
                )
                .unwrap();
                candidate.authority = MemoryAuthority::ExplicitUser;
                barrier.wait();
                store.commit_candidate(&candidate).unwrap();
            }));
        }
        for thread in threads {
            thread.join().unwrap();
        }

        let active = store.list_active().unwrap();
        assert_eq!(
            active.len(),
            1,
            "only one physical current truth may remain"
        );
        assert_eq!(store.effective_active().unwrap(), active);
        assert_eq!(store.list_archived().unwrap().len(), writers);
    }

    #[test]
    fn a_strict_superset_merges_instead_of_duplicating() {
        let (_dir, store) = store();
        let first = parse_durable_fact("以后本项目平台支持固定为 Windows").unwrap();
        store.commit_candidate(&first).unwrap();
        let second =
            parse_durable_fact("以后本项目平台支持固定为 Windows\n同时支持 Linux").unwrap();
        let outcome = store.commit_candidate(&second).unwrap();
        assert_eq!(outcome.operation, AppliedOperation::Merged);
        let active = store.effective_active().unwrap();
        assert_eq!(active.len(), 1, "the two facts become one");
        assert!(active[0].body.contains("Windows") && active[0].body.contains("Linux"));
        assert_eq!(store.list_archived().unwrap().len(), 1, "history kept");
    }

    #[test]
    fn conflicting_values_never_merge() {
        let (_dir, store) = store();
        let first = parse_durable_fact("以后本项目默认模型固定为 flash").unwrap();
        store.commit_candidate(&first).unwrap();
        let second = parse_durable_fact("以后发布探针代号固定为 pro").unwrap();
        // Different subject → different key → create, not merge.
        let outcome = store.commit_candidate(&second).unwrap();
        assert_eq!(outcome.operation, AppliedOperation::Created);
        assert_eq!(store.effective_active().unwrap().len(), 2);
    }

    #[test]
    fn model_inference_cannot_supersede_explicit_user() {
        let (_dir, store) = store();
        let mut explicit = parse_durable_fact("以后本项目平台支持固定为 Windows").unwrap();
        explicit.semantic_key = Some(semantic_key_of("平台支持"));
        explicit.key = Some(semantic_key_of("平台支持"));
        store.commit_candidate(&explicit).unwrap();

        let mut inference = explicit.clone();
        inference.semantic_key = Some(semantic_key_of("平台支持"));
        inference.key = Some(semantic_key_of("平台支持"));
        inference.authority = MemoryAuthority::ModelInference;
        inference.body = "平台支持：只支持 macOS".to_string();

        let decision = decide(&store, &inference, &now_rfc3339()).unwrap();
        assert!(
            matches!(decision.operation, MemoryOperation::NeedsApproval { .. }),
            "{:?}",
            decision.operation
        );
        let outcome = store.commit_candidate(&inference).unwrap();
        assert_eq!(outcome.operation, AppliedOperation::NeedsApproval);
        assert!(
            store.effective_active().unwrap()[0]
                .body
                .contains("Windows")
        );
    }

    #[test]
    fn an_expired_entry_is_not_recallable() {
        let (_dir, store) = store();
        let mut e = crate::new_entry("临时端口", "当前端口 51234", vec![]);
        e.key = Some("k-temp-port".to_string());
        e.expires_at = Some("2000-01-01T00:00:00Z".to_string());
        store.remember_deduplicated(e).unwrap();
        assert!(store.search("端口", 5).unwrap().is_empty());
        let expired = store.expire_due().unwrap();
        assert_eq!(expired.len(), 1);
        assert_eq!(expired[0].status, MemoryStatus::Expired);
    }

    #[test]
    fn provenance_survives_commit() {
        let (_dir, store) = store();
        let fact = parse_durable_fact("以后这个项目的发布探针代号固定为 ORANGE-7319").unwrap();
        let entry = store.commit_candidate(&fact).unwrap().entry.unwrap();
        assert_eq!(entry.authority(), MemoryAuthority::ExplicitUser);
        let provenance = entry.provenance.clone().expect("provenance");
        assert_eq!(provenance.authority, MemoryAuthority::ExplicitUser);
        assert_eq!(provenance.source, "explicit_user_turn");
    }

    /// Updating a historical credential must not copy its plaintext into the
    /// history copy. `forget` and expiry already redact; supersede is another
    /// retention path and follows the same boundary.
    #[test]
    fn superseding_a_legacy_sensitive_entry_redacts_the_history_copy() {
        let (_dir, store) = store();
        let secret = "ghp_abcdefghijklmnopqrstuvwxyz0123";
        let key = semantic_key_of("部署令牌");
        let mut old = crate::new_entry("部署令牌", &format!("token={secret}"), vec![]);
        old.key = Some(key.clone());
        // A credential could only be on disk from a store written before the
        // write boundary existed, so place the file directly.
        std::fs::write(
            store.active_path(&old.id),
            serde_json::to_string_pretty(&old).unwrap(),
        )
        .unwrap();

        let mut candidate = MemoryCandidate::new(
            "部署令牌轮换",
            "部署令牌：rotate",
            CandidateKind::Free,
            Some(key),
            CandidateSource::UserExplicit,
            vec![],
        )
        .unwrap();
        candidate.authority = MemoryAuthority::ExplicitUser;
        let outcome = store.commit_candidate(&candidate).unwrap();
        assert_eq!(outcome.operation, AppliedOperation::Superseded);

        let raw = std::fs::read_to_string(store.archive_path(&old.id)).unwrap();
        assert!(
            !raw.contains(secret),
            "supersede copied the credential into history: {raw}"
        );
        assert!(raw.contains(crate::REDACTED_BODY), "{raw}");
        assert!(store.audit_sensitive().unwrap().is_empty());
    }

    /// A retry of a candidate that predates the current truth must not revert
    /// it. The delayed copy can arrive after a crash replay or a slow queue.
    #[test]
    fn a_delayed_retry_of_an_old_candidate_cannot_revert_current_truth() {
        let (_dir, store) = store();
        let old_candidate =
            parse_durable_fact("以后本项目默认模型固定为 Pro").unwrap();
        store.commit_candidate(&old_candidate).unwrap();
        let current = parse_durable_fact("以后本项目默认模型改为 Flash").unwrap();
        store.commit_candidate(&current).unwrap();

        let mut retried = old_candidate.clone();
        retried.created_at = "2020-01-01T00:00:00Z".to_string();
        let outcome = store.commit_candidate(&retried).unwrap();

        assert_eq!(
            outcome.operation,
            AppliedOperation::Skipped,
            "an older candidate must not supersede newer truth"
        );
        let active = store.effective_active().unwrap();
        assert_eq!(active.len(), 1);
        assert!(active[0].body.contains("Flash"));
    }

    // ---- unified write paths ----

    /// A user write that states a durable assignment uses the same identity as
    /// every other path: the later value replaces the earlier one.
    #[test]
    fn activating_a_durable_assignment_updates_the_same_identity() {
        let (_dir, store) = store();
        let first = store
            .activate(
                "默认模型",
                "以后本项目默认模型固定为 Pro",
                crate::MemoryKind::Preference,
                vec![],
            )
            .unwrap();
        assert!(first.key.is_some(), "identity is established on write");

        let second = store
            .activate(
                "默认模型",
                "以后本项目默认模型改为 Flash",
                crate::MemoryKind::Preference,
                vec![],
            )
            .unwrap();
        assert_eq!(second.key, first.key, "same semantic identity");

        let current = store.effective_active().unwrap();
        assert_eq!(current.len(), 1);
        assert!(current[0].body.contains("Flash"));
        assert_eq!(store.list_archived().unwrap()[0].id, first.id);
        assert_eq!(
            current[0].kind.as_deref(),
            Some("preference"),
            "the caller's kind survives the lifecycle"
        );
    }

    /// The model's `remember` runs the same decision. A conflict with a user
    /// fact is parked for a person, never applied and never dropped.
    #[test]
    fn agent_conflict_with_user_truth_is_queued_not_applied() {
        let (_dir, store) = store();
        let user = parse_durable_fact("以后本项目默认模型固定为 Pro").unwrap();
        store.commit_candidate(&user).unwrap();

        let agent = agent_candidate(
            "项目默认模型",
            "项目默认模型是 Flash",
            MemoryKind::Note,
            vec![],
            "project.default_model",
            None,
        )
        .unwrap();
        let outcome = store.admit_agent_candidate(agent).unwrap();
        assert!(
            matches!(outcome, crate::AgentAdmitOutcome::Queued { .. }),
            "{outcome:?}"
        );
        let active = store.effective_active().unwrap();
        assert_eq!(active.len(), 1);
        assert!(active[0].body.contains("Pro"));
        assert_eq!(store.list_pending().unwrap().len(), 1);
    }

    /// The model may correct its own earlier low-authority record, but only
    /// with new evidence. The entry stays model authority.
    #[test]
    fn agent_self_update_requires_evidence() {
        let (_dir, store) = store();
        let first = agent_candidate(
            "项目默认模型",
            "项目默认模型是 Pro",
            MemoryKind::Note,
            vec![],
            "project.default_model",
            None,
        )
        .unwrap();
        store.admit_agent_candidate(first).unwrap();

        let unsupported = agent_candidate(
            "项目默认模型",
            "项目默认模型是 Flash",
            MemoryKind::Note,
            vec![],
            "project.default_model",
            None,
        )
        .unwrap();
        assert!(matches!(
            store.admit_agent_candidate(unsupported).unwrap(),
            crate::AgentAdmitOutcome::Queued { .. }
        ));
        assert!(store.effective_active().unwrap()[0].body.contains("Pro"));

        let supported = agent_candidate(
            "项目默认模型",
            "项目默认模型是 Flash",
            MemoryKind::Note,
            vec![],
            "project.default_model",
            Some("conversation turn 12".to_string()),
        )
        .unwrap();
        let outcome = store.admit_agent_candidate(supported).unwrap();
        assert!(matches!(
            outcome,
            crate::AgentAdmitOutcome::Applied(CommitOutcome {
                operation: AppliedOperation::Superseded,
                ..
            })
        ));
        let active = store.effective_active().unwrap();
        assert_eq!(active.len(), 1);
        assert!(active[0].body.contains("Flash"));
        assert_eq!(active[0].authority(), MemoryAuthority::ModelInference);
        assert_eq!(
            active[0]
                .provenance
                .as_ref()
                .and_then(|p| p.evidence.as_deref()),
            Some("conversation turn 12")
        );
    }

    /// Restoring history is a user command: it supersedes the current value
    /// and keeps the chain, never leaving two active truths.
    #[test]
    fn restoring_history_supersedes_current_truth() {
        let (_dir, store) = store();
        let first = parse_durable_fact("以后本项目默认模型固定为 Pro").unwrap();
        let old_id = store
            .commit_candidate(&first)
            .unwrap()
            .entry
            .unwrap()
            .id;
        let second = parse_durable_fact("以后本项目默认模型改为 Flash").unwrap();
        store.commit_candidate(&second).unwrap();

        let restored = store.restore(&old_id).unwrap();
        assert_eq!(
            restored.provenance.as_ref().unwrap().source,
            "user_restore"
        );
        assert!(restored.body.contains("Pro"));
        let active = store.effective_active().unwrap();
        assert_eq!(active.len(), 1);
        assert!(active[0].body.contains("Pro"));
        assert!(
            store
                .list_archived()
                .unwrap()
                .iter()
                .any(|entry| entry.body.contains("Flash")),
            "the replaced value stays in history"
        );
    }

    /// A credential-bearing historical record was redacted on the way into
    /// history. Restoring it would either resurrect the secret or silently
    /// restore placeholder text, so both are refused.
    #[test]
    fn restoring_a_redacted_sensitive_history_is_refused() {
        let (_dir, store) = store();
        let secret = "ghp_abcdefghijklmnopqrstuvwxyz0123";
        let entry = crate::new_entry("部署令牌", &format!("token={secret}"), vec![]);
        std::fs::write(
            store.active_path(&entry.id),
            serde_json::to_string_pretty(&entry).unwrap(),
        )
        .unwrap();
        store.forget(&entry.id).unwrap();

        let error = store.restore(&entry.id).unwrap_err();
        assert!(matches!(error, MemoryError::Invalid(_)), "{error}");
        assert!(store.effective_active().unwrap().is_empty());
    }

    /// The always-injected preference block is one instruction per identity,
    /// never the old and the new value side by side.
    #[test]
    fn standing_preferences_never_hold_two_values_for_one_identity() {
        let (_dir, store) = store();
        store
            .activate(
                "默认模型",
                "以后本项目默认模型固定为 Pro",
                MemoryKind::Preference,
                vec![],
            )
            .unwrap();
        store
            .activate(
                "默认模型",
                "以后本项目默认模型改为 Flash",
                MemoryKind::Preference,
                vec![],
            )
            .unwrap();
        let preferences = store.standing_preferences(16).unwrap();
        assert_eq!(preferences.len(), 1);
        assert!(preferences[0].body.contains("Flash"));
        assert!(!preferences[0].body.contains("Pro"));
    }

    /// A fresh statement of an older value is the user changing their mind
    /// back, not a delayed retry. It must still win.
    #[test]
    fn a_fresh_restatement_of_an_older_value_is_not_treated_as_a_retry() {
        let (_dir, store) = store();
        let pro = parse_durable_fact("以后本项目默认模型固定为 Pro").unwrap();
        store.commit_candidate(&pro).unwrap();
        let flash = parse_durable_fact("以后本项目默认模型改为 Flash").unwrap();
        store.commit_candidate(&flash).unwrap();

        let pro_again = parse_durable_fact("以后本项目默认模型改为 Pro").unwrap();
        let outcome = store.commit_candidate(&pro_again).unwrap();
        assert_eq!(outcome.operation, AppliedOperation::Superseded);
        let active = store.effective_active().unwrap();
        assert_eq!(active.len(), 1);
        assert!(active[0].body.contains("Pro"));
    }

    /// Every automatic surface reads the same current truth: an update must
    /// not leave the old value in the catalog, the standing preference block,
    /// the legacy index or query recall.
    #[test]
    fn every_injection_surface_drops_the_superseded_value() {
        let (_dir, store) = store();
        let first = parse_durable_fact("以后本项目默认模型固定为 Pro").unwrap();
        store.commit_candidate(&first).unwrap();
        let second = parse_durable_fact("以后本项目默认模型改为 Flash").unwrap();
        store.commit_candidate(&second).unwrap();

        let catalog = store.catalog_lines(16).unwrap();
        let standing = store.standing_preferences(16).unwrap();
        let index = store.index_lines(16).unwrap();
        let recalled = store.recall("默认模型", 8).unwrap();
        for surface in [
            catalog,
            index,
            recalled
                .iter()
                .map(|(entry, _)| format!("{} {}", entry.id, entry.title))
                .collect::<Vec<_>>()
                .join("\n"),
            standing
                .iter()
                .map(|entry| format!("{} {}", entry.id, entry.title))
                .collect::<Vec<_>>()
                .join("\n"),
        ] {
            assert!(!surface.contains("Pro"), "stale value survived: {surface}");
        }
        assert!(store.effective_active().unwrap()[0].body.contains("Flash"));
    }

    /// Time-bound state must not become a lasting fact through restore.
    #[test]
    fn restoring_time_bound_history_is_refused() {
        let (_dir, store) = store();
        let mut entry = crate::new_entry("临时端口", "当前端口 51234", vec![]);
        entry.scope = crate::MemoryScope::Ephemeral;
        entry.expires_at = Some("2000-01-01T00:00:00Z".into());
        store.remember(entry.clone()).unwrap();
        store.expire_due().unwrap();

        let error = store.restore(&entry.id).unwrap_err();
        assert!(matches!(error, MemoryError::Invalid(_)), "{error}");
        assert!(store.effective_active().unwrap().is_empty());
    }

    #[test]
    fn recall_returns_the_current_truth_only() {
        let (_dir, store) = store();
        let a = parse_durable_fact("以后这个项目的发布探针代号固定为 ORANGE-7319").unwrap();
        store.commit_candidate(&a).unwrap();
        let b = parse_durable_fact("以后发布探针代号改成 BLUE-4821").unwrap();
        store.commit_candidate(&b).unwrap();
        let hits = store.recall("发布探针代号", 5).unwrap();
        assert_eq!(hits.len(), 1);
        assert!(hits[0].0.body.contains("BLUE-4821"));
        assert!(!hits[0].0.body.contains("ORANGE-7319"));
    }
}
