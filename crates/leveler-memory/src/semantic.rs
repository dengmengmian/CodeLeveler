//! Semantic memory-candidate extraction: the pure half.
//!
//! An LLM reads the user's own sentence and answers "what did the user just
//! say?"; it produces [`SemanticCandidate`]s. It never decides whether those
//! candidates are allowed to become durable memory. That decision is this
//! module's job and is entirely deterministic:
//!
//! ```text
//! user message ──► (LLM) ──► SemanticCandidate[]
//!                                  │  validate_semantic_candidate
//!                                  ▼
//!                          MemoryCandidate[]  ──► MemoryCommitDecision
//! ```
//!
//! Nothing here touches the store. The extractor cannot write, supersede,
//! forget or approve anything; it can only propose. In particular the
//! `evidence_span` gate makes "the user said this" a checked fact rather than a
//! model claim: a candidate whose evidence is not found in the current user
//! message is refused, so a model can never invent an ExplicitUser fact.
//!
//! The schema is deliberately minimal and provider-neutral. It is the ONE
//! structured contract every provider adapter's extraction result is parsed
//! into, so no provider-specific shaping leaks into the runtime.

use serde::{Deserialize, Serialize};

use crate::candidates::{CandidateKind, CandidateSource, MemoryCandidate, looks_like_secret};
use crate::candidates::{CandidateOperation, MemoryAuthority, semantic_key_of};

/// Bounds of a single atomic fact. Long enough for a real sentence, short
/// enough that a multi-sentence blob cannot slip through as one memory.
const MAX_FACT_CHARS: usize = 240;
/// Bounds of the quoted user evidence.
const MAX_EVIDENCE_CHARS: usize = 300;
/// Bounds of the subject identity string.
const MAX_SUBJECT_CHARS: usize = 96;
/// Bounds of the structured value of an assignment.
const MAX_VALUE_CHARS: usize = 240;

/// A candidate below this self-reported confidence is not trusted enough to
/// become durable memory without a human. Only consulted when the model
/// reports one; a missing confidence is not a reason to refuse.
const MIN_CONFIDENCE: f32 = 0.5;

/// How far a fact reaches. Only [`Self::Project`] is committable this round;
/// the rest are recognized so the extractor can classify them and the runtime
/// can skip them explicitly instead of treating them as errors.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CandidateScope {
    Project,
    Session,
    Task,
    User,
}

/// Whether the user framed the fact as lasting.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CandidateDurability {
    Durable,
    Temporary,
    Unknown,
}

/// Claude Code-style auto-memory category.
///
/// `Derived` and `TaskState` are rejection buckets. Naming them in the model
/// contract lets the deterministic boundary refuse repository truth and
/// transient progress even when extraction over-produces.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SemanticMemoryType {
    User,
    Feedback,
    Project,
    Reference,
    Derived,
    TaskState,
}

/// What the model thinks should happen. A hint only: the commit decision
/// re-derives the operation from the store, so a mislabeled hint cannot mutate
/// the wrong memory.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OperationHint {
    Create,
    Update,
    Reaffirm,
    Negate,
    Temporary,
    Unknown,
}

/// What the user is DOING with the words in `evidence_span`.
///
/// Authority alone says who spoke; it says nothing about whether the sentence
/// asserted a fact. A suggestion under evaluation, a hypothetical, a quotation
/// or an unclear remark can all be `explicit_user`, yet none of them is a
/// decision. The default is [`Self::Uncertain`] so a candidate from an older
/// extractor, or one missing the field, is never treated as a confirmed fact.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StatementKind {
    /// A settled fact or decision the user states as true.
    Assertion,
    /// An explicit correction of a prior decision.
    Correction,
    /// An explicit refusal of an option or a prior fact.
    Rejection,
    /// An option being evaluated or suggested, not decided.
    Proposal,
    /// A what-if or example, not a real decision.
    Hypothetical,
    /// Words attributed to someone else, or material being shown.
    Quotation,
    /// Hedged, ambiguous or missing classification.
    #[default]
    Uncertain,
}

impl StatementKind {
    /// Whether this statement may become current truth without a person.
    ///
    /// Only a statement the user put forward as their own settled position
    /// qualifies. Evaluation, speculation and reported speech are not
    /// decisions, however explicit the wording sounds.
    pub fn is_confirmed_user_statement(self) -> bool {
        matches!(self, Self::Assertion | Self::Correction | Self::Rejection)
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Assertion => "assertion",
            Self::Correction => "correction",
            Self::Rejection => "rejection",
            Self::Proposal => "proposal",
            Self::Hypothetical => "hypothetical",
            Self::Quotation => "quotation",
            Self::Uncertain => "uncertain",
        }
    }
}

/// One thing the user's sentence expressed, as the extractor understood it.
///
/// Atomic by construction: one fact, one subject, one value. A sentence with
/// three durable statements yields three candidates, so each can later be
/// updated, superseded and recalled independently.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SemanticCandidate {
    /// The fact in one short clause, in the language of the conversation.
    pub fact: String,
    /// A stable identity for the fact's subject, e.g. `project.default_model`.
    /// The runtime normalizes it; it is not trusted as-is.
    pub subject: String,
    /// When this statement restates, replaces, or reverses an existing
    /// decision that was originally recorded under another subject, the model
    /// names that subject here. The runtime resolves it against active memory;
    /// it does not compare wording.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub related_subject: Option<String>,
    /// The exact id of the active memory this statement replaces, from the
    /// `known_identities` supplied with the request. This is how a correction
    /// reaches an entry that has no semantic subject (a legacy direct note).
    /// The runtime resolves the id; it never guesses a target.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub replaces_memory_id: Option<String>,
    /// The value when the fact is an assignment (`Pro`, `required`, `forbidden`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub value: Option<String>,
    /// What the user is doing with this span. Missing means `uncertain`, which
    /// is never auto-committed.
    #[serde(default)]
    pub statement: StatementKind,
    pub scope: CandidateScope,
    pub durability: CandidateDurability,
    pub memory_type: SemanticMemoryType,
    pub authority: MemoryAuthority,
    pub operation_hint: OperationHint,
    /// The user's own words this fact was drawn from. Mandatory: it is the
    /// authorization for an autonomous write, and it is verified against the
    /// current user message before anything is committed.
    pub evidence_span: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub confidence: Option<f32>,
}

/// Why a candidate was refused. Refusal is not a failure: `Temporary` and
/// `Unknown` are the runtime correctly declining to remember something that
/// was never framed as lasting.
#[derive(Debug, Clone, PartialEq)]
pub enum CandidateRejection {
    /// `fact` was empty after trimming.
    EmptyFact,
    /// `fact` was too long to be one atomic statement.
    FactTooLong { chars: usize },
    /// `subject` was empty or too long to be a stable identity.
    InvalidSubject,
    /// Only `explicit_user` may auto-commit this round.
    NotExplicitUser { authority: MemoryAuthority },
    /// Only a durable statement may auto-commit.
    NotDurable { durability: CandidateDurability },
    /// Only project scope may auto-commit this round.
    NotProjectScope { scope: CandidateScope },
    /// Repository-derived truth and task state have authoritative owners
    /// elsewhere and must not be copied into long-term memory.
    NotAutoMemory { memory_type: SemanticMemoryType },
    /// The evidence span was empty or not found in the current user message.
    EvidenceNotFound,
    /// The evidence exists only inside quoted, fenced or blockquoted material:
    /// a pasted log or a discussed phrase, not the user speaking.
    QuotedEvidence,
    /// The candidate text looked like a credential.
    Sensitive,
    /// The model itself reported low confidence.
    LowConfidence { confidence: f32 },
    /// The statement is a proposal, hypothetical, quotation or unclear. Only
    /// the user's own settled statement may become current truth.
    NotAConfirmedStatement { statement: StatementKind },
    /// The candidate's evidence contains the span a direct memory command
    /// already stored. That command has its own deterministic writer, so a
    /// second extracted memory for the same words would be a competing
    /// current truth.
    DirectCommandOwned,
    /// `replaces_memory_id` was not a usable memory id.
    InvalidTarget,
}

impl CandidateRejection {
    /// A stable machine code for tracing.
    pub fn code(&self) -> &'static str {
        match self {
            Self::EmptyFact => "empty_fact",
            Self::FactTooLong { .. } => "fact_too_long",
            Self::InvalidSubject => "invalid_subject",
            Self::NotExplicitUser { .. } => "not_explicit_user",
            Self::NotDurable { .. } => "not_durable",
            Self::NotProjectScope { .. } => "not_project_scope",
            Self::NotAutoMemory { .. } => "not_auto_memory",
            Self::EvidenceNotFound => "evidence_not_found",
            Self::QuotedEvidence => "quoted_evidence",
            Self::Sensitive => "sensitive",
            Self::LowConfidence { .. } => "low_confidence",
            Self::NotAConfirmedStatement { .. } => "not_confirmed",
            Self::DirectCommandOwned => "direct_command_owned",
            Self::InvalidTarget => "invalid_target",
        }
    }

    /// A short human-readable reason for a debug log. Never shown in the
    /// normal UI — a skipped candidate is not an event.
    pub fn reason(&self) -> String {
        match self {
            Self::EmptyFact => "candidate fact was empty".to_string(),
            Self::FactTooLong { chars } => format!("candidate fact was {chars} chars"),
            Self::InvalidSubject => "candidate subject was not a usable identity".to_string(),
            Self::NotExplicitUser { authority } => {
                format!("authority {} may not auto-commit", authority.as_str())
            }
            Self::NotDurable { durability } => format!("durability {durability:?} is not durable"),
            Self::NotProjectScope { scope } => format!("scope {scope:?} is not project"),
            Self::NotAutoMemory { memory_type } => {
                format!("{memory_type:?} belongs to an authoritative source, not auto-memory")
            }
            Self::EvidenceNotFound => {
                "evidence span was not found in the current user message".to_string()
            }
            Self::QuotedEvidence => {
                "evidence span appears only inside pasted or quoted material".to_string()
            }
            Self::Sensitive => "candidate looked like a secret".to_string(),
            Self::LowConfidence { confidence } => {
                format!("model confidence {confidence:.2} was below threshold")
            }
            Self::NotAConfirmedStatement { statement } => {
                format!("a {} is not a confirmed user statement", statement.as_str())
            }
            Self::DirectCommandOwned => {
                "the direct memory writer already stored this span".to_string()
            }
            Self::InvalidTarget => "candidate named an unusable memory id".to_string(),
        }
    }
}

/// Why the extractor's output could not be read at all. Distinct from a
/// rejected candidate: this is a transport/format failure, and the caller
/// falls back to the deterministic path.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum SemanticError {
    #[error("extractor returned no usable JSON")]
    NoJson,
    #[error("extractor returned malformed JSON: {0}")]
    Malformed(String),
    #[error("extractor JSON did not match the candidate schema: {0}")]
    Schema(String),
}

impl From<serde_json::Error> for SemanticError {
    fn from(error: serde_json::Error) -> Self {
        Self::Schema(error.to_string())
    }
}

/// Parse the extractor's raw text into candidates.
///
/// Deliberately lenient about packaging — models wrap JSON in prose or code
/// fences — and strict about the shape once JSON is found. Both a top-level
/// array and `{"candidates": [...]}` are accepted; anything else is a schema
/// error, never a silently empty result (an empty extraction is `[]`, which is
/// a real answer, not a parse failure).
pub fn parse_semantic_candidates(raw: &str) -> Result<Vec<SemanticCandidate>, SemanticError> {
    let cleaned = strip_code_fence(raw.trim());
    let Some(json) = extract_json(cleaned) else {
        return Err(SemanticError::NoJson);
    };
    let value: serde_json::Value =
        serde_json::from_str(json).map_err(|e| SemanticError::Malformed(e.to_string()))?;
    parse_value(value)
}

fn parse_value(value: serde_json::Value) -> Result<Vec<SemanticCandidate>, SemanticError> {
    let array = match value {
        serde_json::Value::Array(items) => items,
        serde_json::Value::Object(mut map) => match map.remove("candidates") {
            Some(serde_json::Value::Array(items)) => items,
            Some(_) => {
                return Err(SemanticError::Schema(
                    "`candidates` was not an array".to_string(),
                ));
            }
            None => {
                return Err(SemanticError::Schema(
                    "expected an array or {\"candidates\": [...]}".to_string(),
                ));
            }
        },
        _ => {
            return Err(SemanticError::Schema(
                "expected an array or {\"candidates\": [...]}".to_string(),
            ));
        }
    };
    array
        .into_iter()
        .map(|item| serde_json::from_value(item).map_err(|e| SemanticError::Schema(e.to_string())))
        .collect()
}

/// Strip a leading/trailing Markdown code fence, if present.
fn strip_code_fence(text: &str) -> &str {
    let trimmed = text.trim();
    let Some(rest) = trimmed.strip_prefix("```") else {
        return trimmed;
    };
    // Drop the info string on the fence line.
    let rest = rest.split_once('\n').map(|(_, body)| body).unwrap_or("");
    let rest = rest.trim_end();
    rest.strip_suffix("```").map(str::trim_end).unwrap_or(rest)
}

/// The first balanced JSON object/array in `text`, ignoring braces inside
/// strings. Returns `None` when there is no candidate JSON at all.
fn extract_json(text: &str) -> Option<&str> {
    let bytes = text.as_bytes();
    let start = bytes.iter().position(|b| *b == b'{' || *b == b'[')?;
    let open = bytes[start];
    let close = if open == b'{' { b'}' } else { b']' };
    let mut depth: i32 = 0;
    let mut in_string = false;
    let mut escaped = false;
    for (offset, byte) in bytes[start..].iter().enumerate() {
        if in_string {
            if escaped {
                escaped = false;
            } else if *byte == b'\\' {
                escaped = true;
            } else if *byte == b'"' {
                in_string = false;
            }
            continue;
        }
        match *byte {
            b'"' => in_string = true,
            b if b == open => depth += 1,
            b if b == close => {
                depth -= 1;
                if depth == 0 {
                    return Some(&text[start..start + offset + 1]);
                }
            }
            _ => {}
        }
    }
    None
}

/// Validate one semantic candidate against the user's own message and convert
/// it to the lifecycle's [`MemoryCandidate`], or explain why it was refused.
///
/// This is the safety gate: every check is deterministic, and the strongest
/// one — [`CandidateRejection::EvidenceNotFound`] — is what stops the model
/// from attributing a fact to the user that the user never wrote.
pub fn validate_semantic_candidate(
    candidate: &SemanticCandidate,
    user_message: &str,
) -> Result<MemoryCandidate, CandidateRejection> {
    let fact = candidate.fact.trim();
    if fact.is_empty() {
        return Err(CandidateRejection::EmptyFact);
    }
    if fact.chars().count() > MAX_FACT_CHARS {
        return Err(CandidateRejection::FactTooLong {
            chars: fact.chars().count(),
        });
    }
    if candidate.authority != MemoryAuthority::ExplicitUser {
        return Err(CandidateRejection::NotExplicitUser {
            authority: candidate.authority,
        });
    }
    if candidate.durability != CandidateDurability::Durable {
        return Err(CandidateRejection::NotDurable {
            durability: candidate.durability,
        });
    }
    if candidate.scope != CandidateScope::Project {
        return Err(CandidateRejection::NotProjectScope {
            scope: candidate.scope,
        });
    }
    if matches!(
        candidate.memory_type,
        SemanticMemoryType::Derived | SemanticMemoryType::TaskState
    ) {
        return Err(CandidateRejection::NotAutoMemory {
            memory_type: candidate.memory_type,
        });
    }

    // A direct memory command owns the span it stored. A candidate quoting
    // that text, or a wider span containing it, restates the same operation;
    // a narrower clause is a separate fact and stays eligible. Ownership is
    // span containment, never text similarity.
    if let Some(body) = crate::parse_direct_memory_command(user_message) {
        let body = body.trim();
        let span = candidate.evidence_span.trim();
        if !body.is_empty() && span.contains(body) {
            return Err(CandidateRejection::DirectCommandOwned);
        }
    }

    // Authority is a claim; the statement kind is what the user is DOING.
    // A proposal, hypothetical, quotation or unclassified span is not a
    // decision even when the model reports `explicit_user`.
    if !candidate.statement.is_confirmed_user_statement() {
        return Err(CandidateRejection::NotAConfirmedStatement {
            statement: candidate.statement,
        });
    }

    if let Some(confidence) = candidate.confidence
        && confidence < MIN_CONFIDENCE
    {
        return Err(CandidateRejection::LowConfidence { confidence });
    }

    if !evidence_matches(&candidate.evidence_span, user_message) {
        return Err(CandidateRejection::EvidenceNotFound);
    }
    if !evidence_matches(
        &candidate.evidence_span,
        &strip_quoted_regions(user_message),
    ) {
        return Err(CandidateRejection::QuotedEvidence);
    }

    if looks_like_secret(fact)
        || looks_like_secret(&candidate.evidence_span)
        || looks_like_secret(&candidate.subject)
        || candidate.value.as_deref().is_some_and(looks_like_secret)
    {
        return Err(CandidateRejection::Sensitive);
    }

    let subject = normalize_subject(&candidate.subject);
    if subject.is_empty() || subject.chars().count() > MAX_SUBJECT_CHARS {
        return Err(CandidateRejection::InvalidSubject);
    }
    let related_subject = candidate
        .related_subject
        .as_deref()
        .map(normalize_subject)
        .filter(|subject| !subject.is_empty());
    if related_subject
        .as_deref()
        .is_some_and(|subject| subject.chars().count() > MAX_SUBJECT_CHARS)
    {
        return Err(CandidateRejection::InvalidSubject);
    }
    if related_subject.as_deref().is_some_and(looks_like_secret) {
        return Err(CandidateRejection::Sensitive);
    }
    let replaces_id = candidate
        .replaces_memory_id
        .as_deref()
        .map(str::trim)
        .filter(|id| !id.is_empty())
        .map(str::to_string);
    // Legacy ids are `mem-<hash>`; Markdown ids are `mem_<uuid>`. Accept the
    // union so a real id from either backend's identity context can be named.
    if replaces_id.as_deref().is_some_and(|id| {
        id.chars().count() > 128
            || !id.chars().all(|character| {
                character.is_ascii_alphanumeric() || character == '-' || character == '_'
            })
    }) {
        return Err(CandidateRejection::InvalidTarget);
    }

    let key = semantic_key_of(&subject);
    let kind = match candidate.memory_type {
        SemanticMemoryType::User | SemanticMemoryType::Feedback => CandidateKind::Preference,
        SemanticMemoryType::Project | SemanticMemoryType::Reference => CandidateKind::Free,
        SemanticMemoryType::Derived | SemanticMemoryType::TaskState => unreachable!(),
    };
    let mut memory = MemoryCandidate::new(
        fact,
        fact,
        kind,
        Some(key.clone()),
        CandidateSource::UserExplicit,
        vec![
            "explicit".to_string(),
            "decision".to_string(),
            "semantic".to_string(),
            format!("memory_type:{:?}", candidate.memory_type).to_ascii_lowercase(),
            format!("subject:{subject}"),
        ],
    )
    .map_err(|_| CandidateRejection::Sensitive)?;
    memory.semantic_key = Some(key);
    memory.related_key = related_subject.as_deref().map(semantic_key_of);
    memory.replaces_id = replaces_id;
    memory.authority = MemoryAuthority::ExplicitUser;
    memory.operation = match candidate.operation_hint {
        OperationHint::Update => CandidateOperation::Update,
        OperationHint::Negate => CandidateOperation::Negate,
        OperationHint::Create
        | OperationHint::Reaffirm
        | OperationHint::Temporary
        | OperationHint::Unknown => CandidateOperation::Create,
    };
    memory.evidence = Some(clip(&candidate.evidence_span, MAX_EVIDENCE_CHARS));
    memory.value = candidate
        .value
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(|value| clip(value, MAX_VALUE_CHARS));
    Ok(memory)
}

/// The accepted and refused halves of one extraction, for tracing and tests.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ValidatedCandidates {
    pub accepted: Vec<MemoryCandidate>,
    /// `(index in the extraction, rejection)` for every refused candidate.
    pub rejected: Vec<(usize, CandidateRejection)>,
}

/// Validate a whole extraction in order, deduplicating by semantic identity so
/// two candidates for the same subject cannot both reach the lifecycle in one
/// turn. The first statement of a subject wins; a later one for the same
/// subject is dropped only when it says the same thing (identical fact), so a
/// genuine second value is kept and the lifecycle's own conflict rules decide.
pub fn validate_semantic_candidates(
    candidates: &[SemanticCandidate],
    user_message: &str,
) -> ValidatedCandidates {
    let mut out = ValidatedCandidates::default();
    for (index, candidate) in candidates.iter().enumerate() {
        match validate_semantic_candidate(candidate, user_message) {
            Ok(memory) => {
                let duplicate = out.accepted.iter().any(|existing| {
                    existing.semantic_key == memory.semantic_key
                        && existing.body.trim() == memory.body.trim()
                });
                if !duplicate {
                    out.accepted.push(memory);
                }
            }
            Err(rejection) => out.rejected.push((index, rejection)),
        }
    }
    out
}

/// Whether `span` occurs in `message` after normalization.
///
/// Normalization folds case and removes whitespace and punctuation only. That
/// tolerates the spacing and quoting differences a model introduces when it
/// quotes the user, and tolerates nothing else: the surviving characters must
/// be exactly the user's, in order, so a paraphrased or invented span is
/// refused.
pub fn evidence_matches(span: &str, message: &str) -> bool {
    let span = normalize_for_match(span);
    if span.is_empty() {
        return false;
    }
    normalize_for_match(message).contains(&span)
}

/// The user's message with quoted material removed: fenced code blocks,
/// inline code spans and blockquote lines. A sentence inside them is being
/// shown or discussed, not asserted as a lasting rule.
fn strip_quoted_regions(message: &str) -> String {
    let mut out = String::new();
    let mut in_fence = false;
    for line in message.lines() {
        let trimmed = line.trim_start();
        if trimmed.starts_with("```") {
            in_fence = !in_fence;
            continue;
        }
        if in_fence || trimmed.starts_with('>') {
            continue;
        }
        // Drop inline code spans on this line.
        let mut in_code = false;
        for character in line.chars() {
            if character == '`' {
                in_code = !in_code;
                continue;
            }
            if !in_code {
                out.push(character);
            }
        }
        out.push('\n');
    }
    out
}

/// Fold to comparable characters: lowercase alphanumerics (Unicode-aware).
fn normalize_for_match(text: &str) -> String {
    text.chars()
        .filter(|c| c.is_alphanumeric())
        .flat_map(|c| c.to_lowercase())
        .collect()
}

/// Normalize a subject's spelling into the shared identity space.
///
/// This is spelling normalization only: case, whitespace and separator
/// punctuation are folded so `project.default_model`, `Project Default Model`
/// and `project-default-model` are one identity. Wording is not interpreted
/// here. Two different names stay two different subjects even when they share
/// a suffix, and no alias table can make one name out of two meanings. The
/// model owns semantic identity; the runtime only guarantees comparison is
/// exact after this fold.
pub fn normalize_subject(raw: &str) -> String {
    // Fold case first: the namespace prefix is documented as `project.`, but
    // the model may capitalize it. Only the dotted form is folded;
    // `project_foo` is its own name.
    let raw = raw.trim().to_lowercase();
    // A stored identity is fed back to writers as the subject to reuse.
    // Mapping the key's separators to `_` would make it a new label, so the
    // extractor's second pass would nest the namespace (`k-k-...`).
    if crate::candidates::is_canonical_key(&raw) {
        return raw;
    }
    let raw = raw.strip_prefix("project.").unwrap_or(raw.as_str());
    let mut normalized = String::with_capacity(raw.len());
    for character in raw.chars() {
        match character {
            c if c.is_whitespace() => normalized.push('_'),
            '.' | '-' | '/' | '\\' => normalized.push('_'),
            c => normalized.push(c),
        }
    }
    normalized
}

fn clip(text: &str, max_chars: usize) -> String {
    text.trim().chars().take(max_chars).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn candidate(fact: &str, subject: &str, evidence: &str) -> SemanticCandidate {
        SemanticCandidate {
            fact: fact.to_string(),
            subject: subject.to_string(),
            related_subject: None,
            replaces_memory_id: None,
            statement: StatementKind::Assertion,
            value: None,
            scope: CandidateScope::Project,
            durability: CandidateDurability::Durable,
            memory_type: SemanticMemoryType::Project,
            authority: MemoryAuthority::ExplicitUser,
            operation_hint: OperationHint::Create,
            evidence_span: evidence.to_string(),
            confidence: None,
        }
    }

    #[test]
    fn parses_a_bare_array_and_an_object_wrapper() {
        let json = r#"[{"fact":"默认模型是 Pro","subject":"project.default_model",
            "scope":"project","durability":"durable","memory_type":"project","authority":"explicit_user",
            "operation_hint":"create","evidence_span":"模型就 Pro"}]"#;
        let items = parse_semantic_candidates(json).unwrap();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].authority, MemoryAuthority::ExplicitUser);

        let wrapped = format!(r#"{{"candidates": {json}}}"#);
        assert_eq!(parse_semantic_candidates(&wrapped).unwrap().len(), 1);
    }

    #[test]
    fn parses_json_inside_a_code_fence_and_prose() {
        let raw = "Here you go:\n```json\n{\"candidates\": []}\n```\n";
        assert!(parse_semantic_candidates(raw).unwrap().is_empty());
    }

    #[test]
    fn empty_extraction_is_an_answer_not_an_error() {
        assert!(parse_semantic_candidates("[]").unwrap().is_empty());
        assert!(
            parse_semantic_candidates(r#"{"candidates":[]}"#)
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn garbage_is_a_parse_error_not_an_empty_result() {
        assert_eq!(
            parse_semantic_candidates("no json here").unwrap_err(),
            SemanticError::NoJson
        );
        assert!(matches!(
            parse_semantic_candidates("{ not valid }").unwrap_err(),
            SemanticError::Malformed(_)
        ));
        assert!(matches!(
            parse_semantic_candidates(r#"{"candidates": 3}"#).unwrap_err(),
            SemanticError::Schema(_)
        ));
    }

    #[test]
    fn missing_memory_type_is_a_schema_error() {
        let raw = r#"[{"fact":"默认模型是 Pro","subject":"project.default_model",
            "scope":"project","durability":"durable","authority":"explicit_user",
            "operation_hint":"create","evidence_span":"模型就 Pro"}]"#;
        assert!(matches!(
            parse_semantic_candidates(raw).unwrap_err(),
            SemanticError::Schema(_)
        ));
    }

    #[test]
    fn parses_dotted_subject_inside_braces_in_strings() {
        let raw = r#"prefix {"candidates":[{"fact":"a } b","subject":"s","scope":"project","durability":"durable","memory_type":"project","authority":"explicit_user","operation_hint":"create","evidence_span":"x"}]} trailing"#;
        let items = parse_semantic_candidates(raw).unwrap();
        assert_eq!(items[0].fact, "a } b");
    }

    #[test]
    fn evidence_must_occur_in_the_message_after_normalization() {
        let msg = "这个项目后面模型就 Pro 吧，别老切来切去了。";
        assert!(evidence_matches("模型就 Pro", msg));
        assert!(evidence_matches(" 模型 就 PRO ", msg));
        assert!(!evidence_matches("用户长期偏好 Rust", msg));
        assert!(!evidence_matches("", msg));
    }

    #[test]
    fn a_direct_command_owns_its_exact_span_not_other_facts_in_the_turn() {
        let msg = "记住：项目使用 pnpm。默认模型使用 Flash。";
        let body = "项目使用 pnpm。默认模型使用 Flash。";
        let owned = candidate("项目使用 pnpm", "project.package_manager", body);
        assert_eq!(
            validate_semantic_candidate(&owned, msg).unwrap_err(),
            CandidateRejection::DirectCommandOwned
        );

        // The direct writer stored the whole body verbatim. A clause quoted
        // from it is a separate fact, not a duplicate of that write.
        let independent = candidate(
            "默认模型使用 Flash",
            "project.default_model",
            "默认模型使用 Flash。",
        );
        assert!(
            validate_semantic_candidate(&independent, msg).is_ok(),
            "{:?}",
            validate_semantic_candidate(&independent, msg).unwrap_err()
        );
    }

    #[test]
    fn a_valid_candidate_becomes_an_explicit_user_memory() {
        let msg = "这个项目后面模型就 Pro 吧";
        let c = candidate("项目默认模型为 Pro", "project.default_model", "模型就 Pro");
        let memory = validate_semantic_candidate(&c, msg).unwrap();
        assert_eq!(memory.authority, MemoryAuthority::ExplicitUser);
        assert_eq!(memory.source, CandidateSource::UserExplicit);
        assert_eq!(
            memory.semantic_key.as_deref(),
            Some(semantic_key_of("default_model").as_str())
        );
        assert_eq!(memory.evidence.as_deref(), Some("模型就 Pro"));
    }

    #[test]
    fn repository_facts_and_task_state_are_not_auto_memory() {
        for (memory_type, message, fact, evidence) in [
            (
                "derived",
                "demoseed 的 RFQ 必须通过 sourcingrepo.Create 创建",
                "RFQ 必须通过 sourcingrepo.Create 创建",
                "RFQ 必须通过 sourcingrepo.Create 创建",
            ),
            (
                "task_state",
                "S12-05 独立验收还剩两个 blocker",
                "S12-05 还剩两个 blocker",
                "S12-05 独立验收还剩两个 blocker",
            ),
        ] {
            let raw = format!(
                r#"{{"candidates":[{{"fact":"{fact}","subject":"project.test","scope":"project","durability":"durable","authority":"explicit_user","operation_hint":"create","evidence_span":"{evidence}","memory_type":"{memory_type}"}}]}}"#
            );
            let candidate = parse_semantic_candidates(&raw).unwrap().remove(0);
            assert!(
                validate_semantic_candidate(&candidate, message).is_err(),
                "{memory_type} must stay out of durable memory"
            );
        }
    }

    /// Pasted material is not the user speaking. A rule quoted inside a code
    /// fence, inline code or blockquote cannot become an explicit user fact.
    #[test]
    fn quoted_or_pasted_material_is_not_user_authority() {
        for message in [
            "日志如下：\n```\n以后默认使用 Flash\n```",
            "请解释 `以后默认使用 Flash` 的含义",
            "> 以后默认使用 Flash\n这句话是什么意思？",
        ] {
            let c = candidate(
                "默认模型是 Flash",
                "project.default_model",
                "以后默认使用 Flash",
            );
            assert_eq!(
                validate_semantic_candidate(&c, message).unwrap_err(),
                CandidateRejection::QuotedEvidence,
                "{message}"
            );
        }
        // The same words spoken normally are still the user's own statement.
        let c = candidate(
            "默认模型是 Flash",
            "project.default_model",
            "以后默认使用 Flash",
        );
        assert!(validate_semantic_candidate(&c, "以后默认使用 Flash，不再用 Pro。").is_ok());
    }

    #[test]
    fn hallucinated_evidence_is_refused() {
        let msg = "今天帮我修一下登录 bug";
        let c = candidate("用户长期偏好 Rust", "language", "用户长期偏好 Rust");
        assert_eq!(
            validate_semantic_candidate(&c, msg).unwrap_err(),
            CandidateRejection::EvidenceNotFound
        );
    }

    #[test]
    fn temporary_and_unknown_durability_are_refused() {
        let msg = "今天先用 Pro";
        let mut c = candidate("今天用 Pro", "project.default_model", "今天先用 Pro");
        c.durability = CandidateDurability::Temporary;
        assert!(matches!(
            validate_semantic_candidate(&c, msg).unwrap_err(),
            CandidateRejection::NotDurable { .. }
        ));
        c.durability = CandidateDurability::Unknown;
        assert!(matches!(
            validate_semantic_candidate(&c, msg).unwrap_err(),
            CandidateRejection::NotDurable { .. }
        ));
    }

    #[test]
    fn non_explicit_authority_is_refused() {
        let msg = "以后用 Rust";
        let mut c = candidate("用 Rust", "language", "以后用 Rust");
        c.authority = MemoryAuthority::ModelInference;
        assert!(matches!(
            validate_semantic_candidate(&c, msg).unwrap_err(),
            CandidateRejection::NotExplicitUser { .. }
        ));
    }

    #[test]
    fn non_project_scope_is_refused() {
        let msg = "以后这个项目用 Rust";
        let mut c = candidate("用 Rust", "language", "以后这个项目用 Rust");
        c.scope = CandidateScope::Task;
        assert!(matches!(
            validate_semantic_candidate(&c, msg).unwrap_err(),
            CandidateRejection::NotProjectScope { .. }
        ));
    }

    #[test]
    fn secrets_are_refused() {
        let msg = "以后 token 用 sk-abcdefghijklmnop 吧";
        let c = candidate(
            "token 是 sk-abcdefghijklmnop",
            "token",
            "token 用 sk-abcdefghijklmnop",
        );
        assert_eq!(
            validate_semantic_candidate(&c, msg).unwrap_err(),
            CandidateRejection::Sensitive
        );

        // A credential can hide in the subject, where it would become part of
        // the identity key and pending id.
        let secret_subject = candidate(
            "deploy key",
            "ghp_abcdefghijklmnopqrstuvwxyz0123",
            "deploy key is set",
        );
        assert_eq!(
            validate_semantic_candidate(&secret_subject, "deploy key is set").unwrap_err(),
            CandidateRejection::Sensitive
        );
    }

    /// Punctuation is not a semantic test. The model's structured candidate is
    /// the unit of a fact; the runtime validates provenance, not how many
    /// clauses a sentence contains.
    #[test]
    fn punctuation_does_not_decide_atomicity() {
        let msg = "以后统一用 Rust；Windows 也必须支持，而且 protected branch 不要直接 push。";
        let mut c = candidate("用 Rust；支持 Windows，禁止 push", "language", msg);
        c.fact = "用 Rust；支持 Windows，禁止 push".to_string();
        let converted =
            validate_semantic_candidate(&c, msg).expect("punctuation must not reject a fact");
        assert_eq!(converted.body, "用 Rust；支持 Windows，禁止 push");
    }

    #[test]
    fn low_confidence_is_refused() {
        let msg = "以后用 Rust";
        let mut c = candidate("用 Rust", "language", "以后用 Rust");
        c.confidence = Some(0.2);
        assert!(matches!(
            validate_semantic_candidate(&c, msg).unwrap_err(),
            CandidateRejection::LowConfidence { .. }
        ));
    }

    /// Normalization folds spelling, not meaning. The same name written with
    /// another case, spacing, or separator is one identity; a name that differs
    /// by a word is a different identity, however similar it looks.
    #[test]
    fn subject_normalization_folds_spelling_only() {
        for subject in [
            "project.default_model",
            "default_model",
            "Default Model",
            "default-model",
            "DEFAULT.MODEL",
        ] {
            assert_eq!(
                semantic_key_of(&normalize_subject(subject)),
                semantic_key_of("default_model"),
                "{subject}"
            );
        }
        for subject in ["model_default", "defaultmodel", "默认模型", "默认"] {
            assert_ne!(
                semantic_key_of(&normalize_subject(subject)),
                semantic_key_of("default_model"),
                "a different spelling of the words is a different subject: {subject}"
            );
        }
    }

    /// A shared suffix is not an identity. Stripping it would make two
    /// different facts update each other.
    #[test]
    fn subjects_with_a_shared_suffix_stay_distinct() {
        assert_ne!(
            semantic_key_of(&normalize_subject("billing_model")),
            semantic_key_of(&normalize_subject("billing_setting"))
        );
        assert_ne!(
            semantic_key_of(&normalize_subject("release_probe_codename")),
            semantic_key_of(&normalize_subject("deploy_window"))
        );
    }

    /// A declared relation is normalized like every subject and carried onto
    /// the candidate for the lifecycle to resolve.
    #[test]
    fn a_related_subject_is_a_structured_identity() {
        let msg = "JSON 响应字段命名也用 snake_case";
        let mut c = candidate(
            "JSON 响应字段命名使用 snake_case",
            "project.json_response_field_naming",
            "JSON 响应字段命名也用 snake_case",
        );
        c.related_subject = Some("Project.API Response Field Naming".to_string());
        c.operation_hint = OperationHint::Reaffirm;
        let memory = validate_semantic_candidate(&c, msg).unwrap();
        assert_eq!(
            memory.related_key.as_deref(),
            Some(semantic_key_of("api_response_field_naming").as_str())
        );
    }

    #[test]
    fn a_batch_keeps_atomic_candidates_and_refuses_the_rest() {
        let msg = "Windows 以后还是得支持，只是这阶段先把 macOS 做完。";
        let batch = vec![
            candidate(
                "Windows support remains required",
                "project.windows.support",
                "Windows 以后还是得支持",
            ),
            {
                let mut t = candidate(
                    "macOS is the current implementation priority",
                    "project.current_priority",
                    "这阶段先把 macOS 做完",
                );
                t.durability = CandidateDurability::Temporary;
                t.operation_hint = OperationHint::Temporary;
                t
            },
        ];
        let validated = validate_semantic_candidates(&batch, msg);
        assert_eq!(validated.accepted.len(), 1);
        assert_eq!(validated.rejected.len(), 1);
        assert!(validated.accepted[0].body.contains("Windows"));
    }

    /// Two candidates that name the same subject with different spelling are
    /// one fact. Identity folds spelling only; different subject strings are
    /// different facts, because meaning belongs to the model.
    #[test]
    fn duplicate_facts_for_the_same_subject_spelling_collapse() {
        let msg = "默认模型用 Pro";
        let batch = vec![
            candidate("默认模型是 Pro", "project.default_model", "默认模型用 Pro"),
            candidate("默认模型是 Pro", "Project.Default-Model", "默认模型用 Pro"),
        ];
        let validated = validate_semantic_candidates(&batch, msg);
        assert_eq!(validated.accepted.len(), 1);
    }
}
