//! Durable-memory lifecycle: identity, authority, decision and mutation.
//!
//! This module is the single owner of "what should happen to memory when a new
//! statement arrives". Storage mutation lives on [`MemoryStore`] (in `lib.rs`);
//! the *decision* lives here so no UI, executor or tool re-invents the rules.
//!
//! The split is deliberate:
//!
//! ```text
//! direct command / extractor / tool candidate
//!         ↓
//! decide(store, candidate)   → MemoryOperation   (pure-ish: reads the store)
//!         ↓
//! commit_candidate(...)      → CommitOutcome     (mutates the store)
//! ```
//!
//! A model never writes the store directly. `remember`/`forget` tools keep
//! their K36 human-approval gate, and a statement without a structured
//! identity is only proposed. An explicit direct memory command, a validated
//! explicit-user fact, or an accepted candidate authorizes an active write.

use std::collections::{HashMap, HashSet};
use std::fs::OpenOptions;

use serde::{Deserialize, Serialize};

use crate::candidates::{CandidateSource, MemoryCandidate};
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

/// Whether a candidate states a fresh fact, corrects an existing one, or
/// reverses it.
///
/// Only a hint: the commit decision re-derives the operation from the store, so
/// a mislabeled candidate cannot overwrite the wrong memory. Negation is
/// separate from Update because a reversal is a change even when the extracted
/// value string happens to match the current one.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CandidateOperation {
    #[default]
    Create,
    Update,
    Negate,
}

/// The operation the decision layer chose. Separated from mutation so it can be
/// asserted on its own.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MemoryOperation {
    /// Nothing with this semantic identity exists yet.
    Create,
    /// An active memory with the same identity must become historical.
    Supersede { existing_id: String },
    /// Every listed active entry states the fact this candidate replaces —
    /// for example the keyed current value plus an unkeyed direct copy left by
    /// an older writer. One new current entry replaces all of them; none may
    /// survive beside it.
    SupersedeMany { existing_ids: Vec<String> },
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

/// Normalize a subject into a stable identity. Deterministic, not semantic:
/// it folds spelling only — case, whitespace and separator characters. It
/// NEVER strips a trailing word: `billing_model` and `billing_setting` are two
/// different facts, and an identity that collapses them makes one silently
/// overwrite the other. Meaning belongs to the model that supplied the
/// subject; the runtime only decides whether two spellings are the same
/// string.
pub fn semantic_key_of(subject: &str) -> String {
    let subject = subject.trim();
    // The extractor receives stored identities as the subject to reuse, so
    // normalization must be idempotent. A string already in the canonical key
    // namespace is its own identity; folding it again would nest `k-k-...`.
    if is_canonical_key(subject) {
        return subject.to_string();
    }
    let normalized: String = subject
        .trim()
        .chars()
        .filter(|c| !c.is_whitespace())
        .map(|c| match c {
            '.' | '-' | '/' | '\\' => '_',
            other => other,
        })
        .flat_map(char::to_lowercase)
        .collect();
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

pub(crate) fn is_canonical_key(value: &str) -> bool {
    value.starts_with("k-")
        && value.len() > 2
        && value
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

/// Identity for a stored entry. Entries written before identities were
/// recorded have none: their old prose is not re-parsed into a guessed
/// subject, because a wrong identity silently overwrites a different fact.
pub(crate) fn durable_entry_key(entry: &MemoryEntry) -> Option<String> {
    entry.key.clone()
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
    let subject = crate::normalize_subject(subject);
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

/// The identity this candidate speaks about. A model may declare which active
/// subject it means to restate or replace; that declared relation wins over
/// its own subject string. No wording is compared.
pub(crate) fn candidate_identity(candidate: &MemoryCandidate) -> Option<&str> {
    candidate
        .related_key
        .as_deref()
        .or(candidate.semantic_key.as_deref())
        .or(candidate.key.as_deref())
}

pub(crate) fn entry_matches_candidate(entry: &MemoryEntry, candidate: &MemoryCandidate) -> bool {
    candidate_identity(candidate)
        .is_some_and(|identity| durable_entry_key(entry).as_deref() == Some(identity))
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
        CandidateSource::RuntimeObserved => "agent_experience",
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

    // The runtime's own extraction may create a verified experience, but an
    // unverified one is a hypothesis. It must be parked for consent even when
    // no entry with this identity exists yet.
    if candidate.source == CandidateSource::RuntimeObserved
        && candidate
            .experience
            .as_ref()
            .is_some_and(|experience| !experience.is_verified())
    {
        return Ok(MemoryDecision {
            operation: MemoryOperation::NeedsApproval {
                reason: "an unverified experience needs a person".to_string(),
            },
            related: Vec::new(),
        });
    }

    // The model may name the exact active memory this statement restates or
    // replaces. The id is a structured target, not a wording guess; it is the
    // one relation that can reach an entry written before semantic keys
    // existed. A missing target is resolved after the identity checks below,
    // so a retry of an already-applied replacement stays idempotent.
    let explicit_target = candidate
        .replaces_id
        .as_deref()
        .and_then(|id| active.iter().find(|entry| entry.id == id).cloned());
    let target_missing = candidate.replaces_id.is_some() && explicit_target.is_none();

    // The model may name the active subject this statement restates or
    // replaces. The runtime only checks that the named subject is active; it
    // never infers a relation from wording. An unresolvable relation is parked,
    // not guessed: using the candidate's own subject would silently rewrite a
    // different fact, and creating it would accumulate a competing Active entry.
    if let Some(related) = candidate.related_key.as_deref()
        && explicit_target.is_none()
        && !active
            .iter()
            .any(|entry| durable_entry_key(entry).as_deref() == Some(related))
    {
        return Ok(MemoryDecision {
            operation: MemoryOperation::NeedsApproval {
                reason: format!(
                    "the related subject `{related}` is not an active memory; \
                     a person must confirm what this statement replaces"
                ),
            },
            related: Vec::new(),
        });
    }

    // An update or negation claims that a decision already exists. If no
    // active memory carries its identity or declared relation, the runtime
    // cannot know which fact it changes; creating it would turn a correction
    // into a second competing truth.
    if matches!(
        candidate.operation,
        CandidateOperation::Update | CandidateOperation::Negate
    ) && explicit_target.is_none()
        && !active
            .iter()
            .any(|entry| entry_matches_candidate(entry, candidate))
    {
        return Ok(MemoryDecision {
            operation: MemoryOperation::NeedsApproval {
                reason: "this updates a decision that is not active; \
                         a person must say which memory it replaces"
                    .to_string(),
            },
            related: Vec::new(),
        });
    }

    if let Some(target) = explicit_target.as_ref() {
        return decide_replacement(store, candidate, target, &active);
    }

    // The named target is gone. If this is a retry after the replacement
    // already landed, the candidate's own identity resolves and the normal
    // idempotency rules apply. Otherwise the runtime has no evidence about
    // which current fact the statement meant.
    if target_missing {
        let identity_resolves = candidate_identity(candidate).is_some_and(|_| {
            active
                .iter()
                .any(|entry| entry_matches_candidate(entry, candidate))
        });
        if !identity_resolves {
            return Ok(MemoryDecision {
                operation: MemoryOperation::NeedsApproval {
                    reason: format!(
                        "the memory named by `{}` is not active; \
                         a person must say what this statement replaces",
                        candidate.replaces_id.as_deref().unwrap_or_default()
                    ),
                },
                related: Vec::new(),
            });
        }
    }

    let identity = candidate_identity(candidate);
    if identity.is_some() {
        let mut related: Vec<MemoryEntry> = active
            .into_iter()
            .filter(|entry| entry_matches_candidate(entry, candidate))
            .collect();
        related.sort_by(|a, b| b.updated_at.cmp(&a.updated_at));
        if let Some(existing) = related.first() {
            // Same identity and same stated value: nothing new, whatever the
            // title says. The value is a structured field the model supplied;
            // the runtime compares fields, never prose.
            let same_stated_value = candidate.operation != CandidateOperation::Negate
                && candidate
                    .value
                    .as_deref()
                    .map(str::trim)
                    .filter(|value| !value.is_empty())
                    .is_some_and(|value| existing.value.as_deref().map(str::trim) == Some(value));
            if existing.body.trim() == candidate.body.trim() || same_stated_value {
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
                        reason: format!("candidate was already superseded by {}", existing.id),
                    },
                    related,
                });
            }
            let incoming = candidate.authority.rank();
            let held = existing.authority().rank();
            let candidate_experience = candidate.experience.as_ref();
            let existing_experience = existing.experience.as_ref();
            // A person's explicit accept/restore may change what an entry IS.
            // Consent is not evidence, so the entry keeps its evidence class,
            // but the person's decision authorizes replacing the other kind.
            let user_decision = matches!(
                candidate.source,
                CandidateSource::UserAccepted | CandidateSource::UserRestore
            );
            // An experience is evidence-bound knowledge, not a replacement for
            // a fact of another kind. Even stronger evidence does not silently
            // turn a user's note/decision/preference into an experience; that
            // is a person's decision.
            if candidate_experience.is_some() && existing_experience.is_none() && !user_decision {
                return Ok(MemoryDecision {
                    operation: MemoryOperation::NeedsApproval {
                        reason:
                            "an experience may not replace another kind of memory without a person"
                                .to_string(),
                    },
                    related,
                });
            }
            if user_decision && candidate_experience.is_some() && existing_experience.is_none() {
                return Ok(MemoryDecision {
                    operation: MemoryOperation::Supersede {
                        existing_id: existing.id.clone(),
                    },
                    related,
                });
            }
            // The runtime's own extraction may only commit a mechanically
            // verified experience. An unverified one is a hypothesis; it can
            // be parked for consent, never made current by this path.
            if let Some(experience) = candidate_experience
                && !experience.is_verified()
                && candidate.source == CandidateSource::RuntimeObserved
            {
                return Ok(MemoryDecision {
                    operation: MemoryOperation::NeedsApproval {
                        reason: "an unverified experience needs a person".to_string(),
                    },
                    related,
                });
            }
            // An observation-only record verifies the observed fact, not the
            // model's cause or advice. A later claim under the same identity
            // can read the same output the other way; replacing the current
            // lesson silently would make the newer reading the only remembered
            // truth. Only a person may choose it.
            if existing_experience.is_some_and(crate::ExperienceRecord::is_observation_only)
                && !user_decision
            {
                return Ok(MemoryDecision {
                    operation: MemoryOperation::NeedsApproval {
                        reason: "an observation-only experience's explanation is model inference; \
                                 a person must replace it"
                            .to_string(),
                    },
                    related,
                });
            }
            // A verified experience may correct an earlier experience under
            // the same identity when it carries new evidence, even at equal
            // authority. This is what lets a later task retire a stale
            // explanation instead of accumulating beside it. It may only do so
            // when the two claims are about the same thing: a generic error
            // line can match two unrelated root causes, and silently replacing
            // one with the other would make the newest explanation the only
            // remembered account.
            if candidate_experience.is_some_and(crate::ExperienceRecord::is_verified)
                && existing_experience.is_some_and(crate::ExperienceRecord::is_verified)
                && candidate
                    .evidence
                    .as_deref()
                    .is_some_and(|evidence| !evidence.trim().is_empty())
            {
                // Replacing current truth needs evidence at least as strong as
                // the claim it retires. An observation-only reading verifies a
                // fact, not a failure that later passed; letting it silently
                // replace a resolution would downgrade the remembered account
                // to model inference. A person may still choose it.
                let incoming_strength = candidate_experience
                    .map(crate::ExperienceRecord::standing)
                    .map(crate::ExperienceStanding::evidence_rank)
                    .unwrap_or_default();
                let held_strength = existing_experience
                    .map(crate::ExperienceRecord::standing)
                    .map(crate::ExperienceStanding::evidence_rank)
                    .unwrap_or_default();
                if !user_decision && incoming_strength < held_strength {
                    return Ok(MemoryDecision {
                        operation: MemoryOperation::NeedsApproval {
                            reason: "weaker evidence cannot replace a stronger experience; \
                                     a person must choose"
                                .to_string(),
                        },
                        related,
                    });
                }
                // The model declared the same subject and both accounts carry
                // mechanically verified evidence, so this is a newer reading of
                // the same failure, not a wording coincidence. The runtime does
                // not compare the prose; identity is the model's structured
                // claim, and evidence decides whether it may become current.
                return Ok(MemoryDecision {
                    operation: MemoryOperation::Supersede {
                        existing_id: existing.id.clone(),
                    },
                    related,
                });
            }
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
                && existing
                    .provenance
                    .as_ref()
                    .map(|value| value.source.as_str())
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

/// Decide a replacement the model bound to an exact active id.
///
/// The id is the only relation that can reach an unkeyed legacy entry, so the
/// authority rules are applied against that entry rather than against a
/// wording guess. Sibling entries with the candidate's own identity, or with
/// the target's exact evidence, are the same current fact and leave current
/// truth together; otherwise one of them would survive beside its replacement.
fn decide_replacement(
    store: &MemoryStore,
    candidate: &MemoryCandidate,
    target: &MemoryEntry,
    active: &[MemoryEntry],
) -> Result<MemoryDecision, MemoryError> {
    // Experiences carry their own evidence-bound standing rules. A generic id
    // replacement must not silently change that class; the identity path in
    // `decide` still handles a corrected experience.
    if target.experience.is_some() || candidate.experience.is_some() {
        return Ok(MemoryDecision {
            operation: MemoryOperation::NeedsApproval {
                reason: "an experience needs a person to change its identity".to_string(),
            },
            related: vec![target.clone()],
        });
    }

    let target_evidence = target
        .provenance
        .as_ref()
        .and_then(|provenance| provenance.evidence.as_deref())
        .map(str::trim)
        .filter(|evidence| !evidence.is_empty());
    let mut related = Vec::new();
    let mut ids = Vec::new();
    for entry in active {
        let same_identity = entry_matches_candidate(entry, candidate);
        let same_evidence = target_evidence.is_some()
            && entry
                .provenance
                .as_ref()
                .and_then(|provenance| provenance.evidence.as_deref())
                .map(str::trim)
                == target_evidence;
        if entry.id == target.id || same_identity || same_evidence {
            if !ids.iter().any(|id| id == &entry.id) {
                ids.push(entry.id.clone());
                related.push(entry.clone());
            }
        }
    }
    related.sort_by_key(|entry| entry.id != target.id);

    let same_value = candidate.operation != CandidateOperation::Negate
        && candidate
            .value
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .is_some_and(|value| target.value.as_deref().map(str::trim) == Some(value));
    if target.body.trim() == candidate.body.trim() || same_value {
        return Ok(MemoryDecision {
            operation: MemoryOperation::Skip {
                reason: format!("memory {} already states this", target.id),
            },
            related,
        });
    }
    if store.candidate_retries_history(candidate, &target.id)? {
        return Ok(MemoryDecision {
            operation: MemoryOperation::Skip {
                reason: format!("candidate was already superseded by {}", target.id),
            },
            related,
        });
    }

    let incoming = candidate.authority.rank();
    let held = target.authority().rank();
    if incoming < held {
        return Ok(MemoryDecision {
            operation: MemoryOperation::NeedsApproval {
                reason: format!(
                    "{} cannot replace the stronger memory {}",
                    candidate.authority.as_str(),
                    target.id
                ),
            },
            related,
        });
    }
    let operation = if ids.len() == 1 {
        MemoryOperation::Supersede {
            existing_id: ids.into_iter().next().expect("one id"),
        }
    } else {
        MemoryOperation::SupersedeMany { existing_ids: ids }
    };

    if matches!(
        candidate.source,
        CandidateSource::UserExplicit | CandidateSource::UserRestore | CandidateSource::UserAccepted
    ) && incoming >= held
    {
        return Ok(MemoryDecision {
            operation,
            related,
        });
    }
    // The model may correct its own earlier tool record with new evidence,
    // but never a fact of another authority class.
    if candidate.source == CandidateSource::AgentProposed
        && target
            .provenance
            .as_ref()
            .map(|provenance| provenance.source.as_str())
            == Some("agent_remember_tool")
        && candidate
            .evidence
            .as_deref()
            .is_some_and(|evidence| !evidence.trim().is_empty())
    {
        return Ok(MemoryDecision {
            operation,
            related,
        });
    }
    Ok(MemoryDecision {
        operation: MemoryOperation::NeedsApproval {
            reason: format!(
                "{} cannot replace {} without a person",
                candidate.authority.as_str(),
                target.authority().as_str()
            ),
        },
        related,
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
            MemoryOperation::SupersedeMany { existing_ids } => {
                // New-first: the replacement is durable before any old truth
                // moves to history, so a crash never leaves zero current
                // entries. `reconcile_active_truths` follows the `supersedes`
                // link on the next commit and finishes the historical half.
                let mut olds = Vec::with_capacity(existing_ids.len());
                for id in &existing_ids {
                    olds.push(self.read_active(id)?);
                }
                let new = self.write_candidate(candidate, olds.first())?;
                let mut archived = Vec::with_capacity(olds.len());
                for old in &olds {
                    archived.push(self.mark_superseded_unlocked(old, &new.id, now)?);
                }
                Ok(CommitOutcome {
                    operation: AppliedOperation::Superseded,
                    entry: Some(new),
                    superseded: archived,
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
        ) && let Some(entry) = outcome.entry.as_ref()
            && let Some(key) = candidate
                .semantic_key
                .as_deref()
                .or(candidate.key.as_deref())
        {
            self.settle_pending_for_commit(entry, key)?;
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
                    .is_none_or(|at| candidate.created_at.as_str() < at));
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
        // Finish a replacement whose new entry was durable but whose old
        // copy was not yet moved: the `supersedes` link is the durable record
        // of that intent. Keyed duplicates are healed by the grouping below;
        // this also covers an unkeyed target that a correction named by id.
        let mut active: Vec<MemoryEntry> = self
            .list_active()?
            .into_iter()
            .filter(|entry| {
                entry.effective_status() == MemoryStatus::Active && !entry.is_expired_at(now)
            })
            .collect();
        active.sort_by(|a, b| {
            b.updated_at
                .cmp(&a.updated_at)
                .then_with(|| b.id.cmp(&a.id))
        });
        let mut healed: HashSet<String> = HashSet::new();
        for entry in &active {
            if healed.contains(&entry.id) {
                continue;
            }
            let Some(old_id) = entry.supersedes.as_deref() else {
                continue;
            };
            if old_id == entry.id || healed.contains(old_id) {
                continue;
            }
            let Some(old) = active.iter().find(|candidate| candidate.id == old_id) else {
                continue;
            };
            self.mark_superseded_unlocked(old, &entry.id, now)?;
            healed.insert(old_id.to_string());
        }

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
        // Restoring an experience restores its evidence class with it. The
        // command is the person's choice, but it does not add evidence.
        candidate.authority = if archived.experience.is_some() {
            archived.authority()
        } else {
            MemoryAuthority::ExplicitUser
        };
        candidate.experience = archived.experience.clone();
        candidate.evidence = Some(format!("restored from memory {}", archived.id));
        let outcome = self.apply_candidate_locked(&candidate, &now_rfc3339())?;
        match outcome.operation {
            AppliedOperation::Created | AppliedOperation::Superseded | AppliedOperation::Merged => {
                outcome.entry.ok_or_else(|| {
                    MemoryError::Invalid("restore produced no current entry".to_string())
                })
            }
            AppliedOperation::Skipped => {
                let current = self.effective_active()?.into_iter().find(|entry| {
                    key.as_deref()
                        .is_some_and(|key| durable_entry_key(entry).as_deref() == Some(key))
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
            .filter_map(|(key, group)| current_truth_of(&group).map(|winner| (key, winner.clone())))
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

    /// A structured explicit-user fact. The model owns the subject; the test
    /// supplies it directly, so no prose is parsed into identity anywhere.
    fn user_fact(subject: &str, body: &str) -> MemoryCandidate {
        let key = semantic_key_of(&crate::normalize_subject(subject));
        let mut candidate = MemoryCandidate::new(
            body,
            body,
            crate::CandidateKind::Free,
            Some(key.clone()),
            CandidateSource::UserExplicit,
            vec![],
        )
        .unwrap();
        candidate.semantic_key = Some(key);
        candidate.authority = MemoryAuthority::ExplicitUser;
        candidate
    }

    fn semantic_fact(fact: &str, subject: &str, evidence: &str) -> crate::SemanticCandidate {
        crate::SemanticCandidate {
            fact: fact.to_string(),
            subject: subject.to_string(),
            related_subject: None,
            replaces_memory_id: None,
            statement: crate::StatementKind::Assertion,
            value: None,
            scope: crate::CandidateScope::Project,
            durability: crate::CandidateDurability::Durable,
            memory_type: crate::SemanticMemoryType::Project,
            authority: MemoryAuthority::ExplicitUser,
            operation_hint: crate::OperationHint::Create,
            evidence_span: evidence.to_string(),
            confidence: Some(0.9),
        }
    }

    fn applied(outcome: crate::AdmitOutcome) -> AppliedOperation {
        match outcome {
            crate::AdmitOutcome::Applied(commit) => commit.operation,
            crate::AdmitOutcome::Proposed(other) => {
                panic!("candidate was not committed: {other:?}")
            }
        }
    }

    fn admit_fact(store: &MemoryStore, fact: &str, subject: &str) -> crate::AdmitOutcome {
        let validated =
            crate::validate_semantic_candidates(&[semantic_fact(fact, subject, fact)], fact);
        assert_eq!(
            validated.accepted.len(),
            1,
            "fixture must validate: {:?}",
            validated.rejected
        );
        store
            .admit_candidate(validated.accepted[0].clone())
            .unwrap()
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

    /// A suffix is part of the subject's name, not decoration. Stripping it
    /// would make two different facts update each other.
    #[test]
    fn suffix_words_are_part_of_the_subject() {
        assert_ne!(
            semantic_key_of(&crate::normalize_subject("billing_model")),
            semantic_key_of(&crate::normalize_subject("billing_setting"))
        );
        assert_ne!(
            semantic_key_of(&crate::normalize_subject("default_model")),
            semantic_key_of(&crate::normalize_subject("default_model_setting"))
        );
    }

    /// A stored identity is fed back to writers as the subject to reuse.
    /// The extractor composes `semantic_key_of(normalize_subject(..))`, so a
    /// second pass must produce the same key; folding it again would nest the
    /// namespace (`k-k-...`) and split one fact into two.
    #[test]
    fn semantic_keys_are_idempotent() {
        for subject in ["deploy.channel", "package_manager", "平台支持"] {
            let key = semantic_key_of(&crate::normalize_subject(subject));
            let reused = semantic_key_of(&crate::normalize_subject(&key));
            assert_eq!(reused, key, "subject: {subject}");
        }
    }

    #[test]
    fn a_correction_and_its_original_share_one_key() {
        let a = user_fact(
            "release_probe_codename",
            "以后这个项目的发布探针代号固定为 ORANGE-7319",
        );
        let mut b = user_fact("release_probe_codename", "以后发布探针代号改成 BLUE-4821");
        b.operation = CandidateOperation::Update;
        assert_eq!(a.semantic_key, b.semantic_key);
        assert_eq!(b.operation, CandidateOperation::Update);
    }

    #[test]
    fn explicit_create_then_supersede_leaves_one_current_truth() {
        let (_dir, store) = store();
        let first = user_fact(
            "release_probe_codename",
            "以后这个项目的发布探针代号固定为 ORANGE-7319",
        );
        let created = store.commit_candidate(&first).unwrap();
        assert_eq!(created.operation, AppliedOperation::Created);
        let first_id = created.entry.unwrap().id;
        assert_eq!(store.effective_active().unwrap().len(), 1);

        let second = user_fact("release_probe_codename", "以后发布探针代号改成 BLUE-4821");
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
        let fact = user_fact(
            "release_probe_codename",
            "以后这个项目的发布探针代号固定为 ORANGE-7319",
        );
        store.commit_candidate(&fact).unwrap();
        let again = store.commit_candidate(&fact).unwrap();
        assert_eq!(again.operation, AppliedOperation::Skipped);
        assert_eq!(store.effective_active().unwrap().len(), 1);
    }

    /// The acceptance fixture: one user constraint, extracted twice with a
    /// different model-generated subject because the model described the
    /// object as an API response once and a JSON response the next time.
    ///
    /// The second candidate declares which subject it restates. The runtime
    /// resolves that declared relation; it never guesses one from the wording.
    /// The two statements therefore leave one Active memory and no pending
    /// question.
    #[test]
    fn a_declared_relation_unites_two_spellings_of_one_constraint() {
        let (_dir, store) = store();
        let message =
            "以后 API 响应字段命名统一使用 snake_case，JSON 响应字段命名统一使用 snake_case";
        let mut first = semantic_fact(
            "API 响应字段命名统一使用 snake_case",
            "project.api_response_field_naming",
            "API 响应字段命名统一使用 snake_case",
        );
        first.value = Some("snake_case".to_string());
        let mut second = semantic_fact(
            "JSON 响应字段命名统一使用 snake_case",
            "project.json_response_field_naming",
            "JSON 响应字段命名统一使用 snake_case",
        );
        second.value = Some("snake_case".to_string());
        second.related_subject = Some("project.api_response_field_naming".to_string());
        let validated = crate::validate_semantic_candidates(&[first, second], message);
        assert_eq!(
            validated.accepted.len(),
            2,
            "the extraction keeps both wordings; the model declares the relation"
        );

        let first = store
            .admit_candidate(validated.accepted[0].clone())
            .unwrap();
        assert_eq!(applied(first), AppliedOperation::Created);
        let second = store
            .admit_candidate(validated.accepted[1].clone())
            .unwrap();
        assert_eq!(
            applied(second),
            AppliedOperation::Skipped,
            "the same constraint stated in other words is not a second memory"
        );

        let active = store.effective_active().unwrap();
        assert_eq!(active.len(), 1);
        assert!(active[0].body.contains("snake_case"));
        assert_eq!(active[0].authority(), MemoryAuthority::ExplicitUser);
        assert_eq!(
            store.list_archived().unwrap().len(),
            0,
            "a duplicate is skipped, not turned into history"
        );
        assert!(
            store.list_pending().unwrap().is_empty(),
            "a duplicate must not ask the user to settle it"
        );
    }

    /// The runtime's own tags are a structured protocol: the model may not
    /// smuggle a second `kind:` or `subject:` into the candidate.
    #[test]
    fn agent_candidate_owns_its_structured_tags() {
        let candidate = agent_candidate(
            "响应字段命名",
            "响应字段命名使用 snake_case",
            MemoryKind::Note,
            vec![
                "kind:preference".to_string(),
                "subject:other".to_string(),
                "kept".to_string(),
            ],
            "response_field_naming",
            None,
        )
        .unwrap();
        assert!(!candidate.tags.iter().any(|tag| tag == "kind:preference"));
        assert!(!candidate.tags.iter().any(|tag| tag == "subject:other"));
        assert!(candidate.tags.iter().any(|tag| tag == "kept"));
        assert!(
            candidate.tags.iter().any(|tag| tag == "kind:note"),
            "{:?}",
            candidate.tags
        );
        assert!(
            candidate
                .tags
                .iter()
                .any(|tag| tag == "subject:response_field_naming"),
            "{:?}",
            candidate.tags
        );
    }

    /// A shared prefix is not one fact. `billing_model` and `billing_setting`
    /// stay two identities through the whole lifecycle: committing both
    /// leaves two current entries and no implicit update.
    #[test]
    fn a_shared_prefix_never_merges_two_facts() {
        let (_dir, store) = store();
        let model = admit_fact(&store, "计费模型使用按量计费", "billing_model");
        assert_eq!(applied(model), AppliedOperation::Created);
        let setting = admit_fact(&store, "计费设置允许月结", "billing_setting");
        assert_eq!(applied(setting), AppliedOperation::Created);
        assert_eq!(store.effective_active().unwrap().len(), 2);
        assert!(store.list_pending().unwrap().is_empty());
    }

    /// Two languages are two spellings of a person's statement, not evidence
    /// that two declared subjects are one. Without a structured relation an
    /// English fact never updates a Chinese fact.
    #[test]
    fn different_languages_do_not_create_an_implicit_update() {
        let (_dir, store) = store();
        let chinese = admit_fact(&store, "默认模型使用 Pro", "default_model");
        assert_eq!(applied(chinese), AppliedOperation::Created);
        let english = admit_fact(
            &store,
            "The release probe codename is ORANGE-7319",
            "release_probe",
        );
        assert_eq!(applied(english), AppliedOperation::Created);
        assert_eq!(store.effective_active().unwrap().len(), 2);
        assert!(store.list_archived().unwrap().is_empty());
    }

    /// Wording is not identity. Without a declared relation the runtime must
    /// not merge two subjects, however similar the prose looks: the model owns
    /// meaning, and a surface match is not evidence that two facts are one.
    #[test]
    fn wording_alone_never_unites_two_subjects() {
        let (_dir, store) = store();
        let first = admit_fact(
            &store,
            "API 响应字段命名统一使用 snake_case",
            "project.api_response_field_naming",
        );
        assert_eq!(applied(first), AppliedOperation::Created);

        let second = admit_fact(
            &store,
            "JSON 响应字段命名统一使用 snake_case",
            "project.json_response_field_naming",
        );
        assert_eq!(
            applied(second),
            AppliedOperation::Created,
            "distinct declared subjects stay distinct; merging needs a relation"
        );
        assert_eq!(store.effective_active().unwrap().len(), 2);
        assert!(store.list_pending().unwrap().is_empty());
    }

    /// A different predicate on the same subject is a different constraint
    /// and must remain a second, independent memory.
    #[test]
    fn a_different_constraint_on_the_same_topic_is_not_merged() {
        let (_dir, store) = store();
        admit_fact(
            &store,
            "API 响应字段命名统一使用 snake_case",
            "project.api_response_field_naming",
        );
        let other = admit_fact(
            &store,
            "API 响应字段排序统一使用字母序",
            "project.api_response_field_order",
        );
        assert_eq!(applied(other), AppliedOperation::Created);
        let active = store.effective_active().unwrap();
        assert_eq!(active.len(), 2, "both constraints stay current");
    }

    /// A changed value for the same identity is not a duplicate: the user's
    /// new statement is current, the old value becomes history, and nothing is
    /// left asking for consent. The relation is the shared declared subject,
    /// not a similarity between the two sentences.
    #[test]
    fn a_changed_value_for_the_same_identity_supersedes() {
        let (_dir, store) = store();
        let first = user_fact("response_field_naming", "响应字段命名使用 snake_case");
        store.commit_candidate(&first).unwrap();
        let mut second = user_fact("response_field_naming", "响应字段命名使用 camelCase");
        second.value = Some("camelCase".to_string());
        let outcome = store.commit_candidate(&second).unwrap();
        assert_eq!(outcome.operation, AppliedOperation::Superseded);
        let active = store.effective_active().unwrap();
        assert_eq!(active.len(), 1);
        assert!(active[0].body.contains("camelCase"));
        let archived = store.list_archived().unwrap();
        assert_eq!(archived.len(), 1);
        assert!(archived[0].body.contains("snake_case"));
        assert!(store.list_pending().unwrap().is_empty());
    }

    /// The same value with the opposite polarity from a weaker authority is a
    /// contradiction, not a duplicate: only the person may replace their own
    /// decision.
    #[test]
    fn opposite_polarity_is_a_conflict_not_a_duplicate() {
        let (_dir, store) = store();
        let mut decision = user_fact("response_field_naming", "响应字段命名不要使用 camelCase");
        decision.value = Some("camelCase".to_string());
        store.commit_candidate(&decision).unwrap();

        let mut agent = agent_candidate(
            "响应字段命名",
            "响应字段命名使用 camelCase",
            MemoryKind::Note,
            vec![],
            "response_field_naming",
            None,
        )
        .unwrap();
        agent.value = Some("camelCase".to_string());
        agent.operation = CandidateOperation::Negate;
        let outcome = store.admit_agent_candidate(agent).unwrap();
        assert!(matches!(outcome, crate::AgentAdmitOutcome::Queued { .. }));
        assert_eq!(store.effective_active().unwrap().len(), 1);
        assert_eq!(store.list_pending().unwrap().len(), 1);
    }

    /// Identity is durable: a reopened store reaches the same conclusion,
    /// which is what a resumed session or another process sees. The second
    /// statement is recognised as the same fact by its declared subject and
    /// structured value, not by a repeated sentence.
    #[test]
    fn a_duplicate_is_still_recognised_after_reopening_the_store() {
        let (dir, store) = store();
        let mut first = user_fact(
            "response_field_naming",
            "API 响应字段命名统一使用 snake_case",
        );
        first.value = Some("snake_case".to_string());
        store.commit_candidate(&first).unwrap();
        drop(store);

        let reopened = MemoryStore::open(dir.path()).unwrap();
        let mut second = user_fact("response_field_naming", "字段命名约定：snake_case");
        second.value = Some("snake_case".to_string());
        let outcome = reopened.commit_candidate(&second).unwrap();
        assert_eq!(outcome.operation, AppliedOperation::Skipped);
        assert_eq!(reopened.effective_active().unwrap().len(), 1);
    }

    /// Two processes (or two threads) may extract the same constraint at the
    /// same time. The lifecycle lock serializes them; exactly one becomes
    /// current and the others are idempotent skips.
    #[test]
    fn concurrent_wordings_of_one_constraint_leave_one_active() {
        use std::sync::{Arc, Barrier};

        let (_dir, store) = store();
        let writers = 8;
        let barrier = Arc::new(Barrier::new(writers));
        let mut threads = Vec::new();
        for index in 0..writers {
            let store = store.clone();
            let barrier = Arc::clone(&barrier);
            threads.push(std::thread::spawn(move || {
                let (fact, subject) = if index % 2 == 0 {
                    (
                        "API 响应字段命名统一使用 snake_case",
                        "response_field_naming",
                    )
                } else {
                    (
                        "JSON 响应字段命名统一使用 snake_case",
                        "response_field_naming",
                    )
                };
                let mut semantic = semantic_fact(fact, subject, fact);
                semantic.value = Some("snake_case".to_string());
                let validated = crate::validate_semantic_candidates(&[semantic], fact);
                let candidate = validated.accepted[0].clone();
                barrier.wait();
                store.commit_candidate(&candidate).unwrap()
            }));
        }
        let mut created = 0;
        let mut skipped = 0;
        for thread in threads {
            match thread.join().unwrap().operation {
                AppliedOperation::Created => created += 1,
                AppliedOperation::Skipped => skipped += 1,
                other => panic!("unexpected operation: {other:?}"),
            }
        }
        assert_eq!(created, 1);
        assert_eq!(skipped, writers - 1);
        assert_eq!(store.effective_active().unwrap().len(), 1);
    }

    /// Two projects have separate stores; the same wording in each is one
    /// memory per project, not a cross-project duplicate.
    #[test]
    fn different_projects_keep_their_own_memory() {
        let (_first_dir, first) = store();
        let (_second_dir, second) = store();
        let fact = "API 响应字段命名统一使用 snake_case";
        assert_eq!(
            applied(admit_fact(
                &first,
                fact,
                "project.api_response_field_naming"
            )),
            AppliedOperation::Created
        );
        assert_eq!(
            applied(admit_fact(
                &second,
                fact,
                "project.api_response_field_naming"
            )),
            AppliedOperation::Created
        );
        assert_eq!(first.effective_active().unwrap().len(), 1);
        assert_eq!(second.effective_active().unwrap().len(), 1);
    }

    /// A direct write is a plain create: the runtime does not parse its prose
    /// into an identity, so a differently worded write is a second memory
    /// rather than a silent replacement.
    #[test]
    fn a_direct_write_never_replaces_another_by_wording() {
        let (_dir, store) = store();
        let first = store
            .activate(
                "API 响应字段命名",
                "以后 API 响应字段命名统一使用 snake_case",
                MemoryKind::Preference,
                vec![],
            )
            .unwrap();
        assert!(first.key.is_none(), "no identity is parsed from prose");
        let second = store
            .activate(
                "JSON 响应字段命名",
                "以后 JSON 响应字段命名统一使用 snake_case",
                MemoryKind::Preference,
                vec![],
            )
            .unwrap();
        assert_ne!(second.id, first.id);
        let active = store.effective_active().unwrap();
        assert_eq!(active.len(), 2);
        assert!(store.list_archived().unwrap().is_empty());
    }

    /// A person accepting the parked conflicting value is the choice the
    /// autonomous path was not allowed to make. The old truth becomes
    /// history; it is archived, never deleted.
    #[test]
    fn accepting_a_conflicting_value_replaces_the_old_truth_and_keeps_history() {
        let (_dir, store) = store();
        let mut decision = user_fact("response_field_naming", "响应字段命名使用 snake_case");
        decision.value = Some("snake_case".to_string());
        store.commit_candidate(&decision).unwrap();

        // The model proposes the same identity with a different value. It has
        // lower authority, so the lifecycle parks it instead of overwriting.
        let mut agent = agent_candidate(
            "响应字段命名",
            "响应字段命名使用 camelCase",
            MemoryKind::Note,
            vec![],
            "response_field_naming",
            None,
        )
        .unwrap();
        agent.value = Some("camelCase".to_string());
        let pending = match store.admit_agent_candidate(agent).unwrap() {
            crate::AgentAdmitOutcome::Queued { candidate, .. } => candidate,
            other => panic!("expected a parked conflict: {other:?}"),
        };

        let accepted = store.accept(&pending.id).unwrap();
        assert!(accepted.body.contains("camelCase"));
        let active = store.effective_active().unwrap();
        assert_eq!(active.len(), 1);
        assert!(active[0].body.contains("camelCase"));
        let archived = store.list_archived().unwrap();
        assert_eq!(archived.len(), 1);
        assert!(archived[0].body.contains("snake_case"));
        assert_eq!(
            archived[0].superseded_by.as_deref(),
            Some(active[0].id.as_str())
        );
    }

    /// An explicit write establishes the current truth for an identity, so an
    /// older unapproved proposal for the same identity is stale. It is
    /// cleared as part of the write, and no second accept is needed.
    #[test]
    fn an_explicit_write_clears_a_stale_equivalent_proposal() {
        let (_dir, store) = store();
        let fact = "响应字段命名统一使用 snake_case";
        let mut semantic = semantic_fact(fact, "response_field_naming", fact);
        semantic.value = Some("snake_case".to_string());
        let validated = crate::validate_semantic_candidates(&[semantic], fact);
        let pending = store.propose(validated.accepted[0].clone()).unwrap();
        let pending = match pending {
            crate::ProposeOutcome::Pending(candidate) => candidate,
            other => panic!("expected pending: {other:?}"),
        };
        let mut active = user_fact("response_field_naming", "字段命名约定：snake_case");
        active.value = Some("snake_case".to_string());
        store.commit_candidate(&active).unwrap();

        assert_eq!(store.effective_active().unwrap().len(), 1);
        assert!(store.list_archived().unwrap().is_empty());
        assert!(
            store.read_pending(&pending.id).is_err(),
            "the superseded proposal must not remain acceptable"
        );
        assert!(store.list_pending().unwrap().is_empty());
    }

    #[test]
    fn retry_reconciles_crash_after_new_truth_became_durable() {
        let (_dir, store) = store();
        let first = user_fact(
            "release_probe_codename",
            "以后这个项目的发布探针代号固定为 ORANGE-7319",
        );
        let old = store
            .commit_candidate(&first)
            .unwrap()
            .entry
            .expect("created entry");
        let second = user_fact("release_probe_codename", "以后发布探针代号改成 BLUE-4821");

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
            crate::CandidateKind::Free,
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
                    crate::CandidateKind::Free,
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
        let first = user_fact("platform_support", "以后本项目平台支持固定为 Windows");
        store.commit_candidate(&first).unwrap();
        let second = user_fact(
            "platform_support",
            "以后本项目平台支持固定为 Windows\n同时支持 Linux",
        );
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
        let first = user_fact("default_model", "以后本项目默认模型固定为 flash");
        store.commit_candidate(&first).unwrap();
        let second = user_fact("release_probe_codename", "以后发布探针代号固定为 pro");
        // Different subject → different key → create, not merge.
        let outcome = store.commit_candidate(&second).unwrap();
        assert_eq!(outcome.operation, AppliedOperation::Created);
        assert_eq!(store.effective_active().unwrap().len(), 2);
    }

    #[test]
    fn model_inference_cannot_supersede_explicit_user() {
        let (_dir, store) = store();
        let mut explicit = user_fact("platform_support", "以后本项目平台支持固定为 Windows");
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
        let fact = user_fact(
            "release_probe_codename",
            "以后这个项目的发布探针代号固定为 ORANGE-7319",
        );
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
            crate::CandidateKind::Free,
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
        let old_candidate = user_fact("default_model", "以后本项目默认模型固定为 Pro");
        store.commit_candidate(&old_candidate).unwrap();
        let current = user_fact("default_model", "以后本项目默认模型改为 Flash");
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

    /// A direct write is a plain create. The runtime does not parse an
    /// identity out of its prose, so a later direct write is a second memory
    /// with its own history rather than a silent replacement.
    #[test]
    fn a_direct_write_does_not_parse_an_identity_from_prose() {
        let (_dir, store) = store();
        let first = store
            .activate(
                "默认模型",
                "以后本项目默认模型固定为 Pro",
                crate::MemoryKind::Preference,
                vec![],
            )
            .unwrap();
        assert!(first.key.is_none(), "no identity is parsed from prose");

        let second = store
            .activate(
                "默认模型",
                "以后本项目默认模型改为 Flash",
                crate::MemoryKind::Preference,
                vec![],
            )
            .unwrap();
        assert!(second.key.is_none());
        assert_ne!(second.id, first.id);

        let current = store.effective_active().unwrap();
        assert_eq!(current.len(), 2);
        assert!(
            store.list_archived().unwrap().is_empty(),
            "nothing was silently replaced"
        );
        assert!(
            current
                .iter()
                .all(|entry| entry.kind.as_deref() == Some("preference")),
            "the caller's kind survives"
        );
    }

    /// The model's `remember` runs the same decision. A conflict with a user
    /// fact is parked for a person, never applied and never dropped.
    #[test]
    fn agent_conflict_with_user_truth_is_queued_not_applied() {
        let (_dir, store) = store();
        let user = user_fact("default_model", "以后本项目默认模型固定为 Pro");
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
        let first = user_fact("default_model", "以后本项目默认模型固定为 Pro");
        let old_id = store.commit_candidate(&first).unwrap().entry.unwrap().id;
        let second = user_fact("default_model", "以后本项目默认模型改为 Flash");
        store.commit_candidate(&second).unwrap();

        let restored = store.restore(&old_id).unwrap();
        assert_eq!(restored.provenance.as_ref().unwrap().source, "user_restore");
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
        let mut pro = user_fact("default_model", "以后本项目默认模型固定为 Pro");
        pro.kind = crate::CandidateKind::Preference;
        store.commit_candidate(&pro).unwrap();
        let mut flash = user_fact("default_model", "以后本项目默认模型改为 Flash");
        flash.kind = crate::CandidateKind::Preference;
        store.commit_candidate(&flash).unwrap();
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
        let pro = user_fact("default_model", "以后本项目默认模型固定为 Pro");
        store.commit_candidate(&pro).unwrap();
        let flash = user_fact("default_model", "以后本项目默认模型改为 Flash");
        store.commit_candidate(&flash).unwrap();

        let pro_again = user_fact("default_model", "以后本项目默认模型改为 Pro");
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
        let first = user_fact("default_model", "以后本项目默认模型固定为 Pro");
        store.commit_candidate(&first).unwrap();
        let second = user_fact("default_model", "以后本项目默认模型改为 Flash");
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
        let a = user_fact(
            "release_probe_codename",
            "以后这个项目的发布探针代号固定为 ORANGE-7319",
        );
        store.commit_candidate(&a).unwrap();
        let b = user_fact("release_probe_codename", "以后发布探针代号改成 BLUE-4821");
        store.commit_candidate(&b).unwrap();
        let hits = store.recall("发布探针代号", 5).unwrap();
        assert_eq!(hits.len(), 1);
        assert!(hits[0].0.body.contains("BLUE-4821"));
        assert!(!hits[0].0.body.contains("ORANGE-7319"));
    }
}
