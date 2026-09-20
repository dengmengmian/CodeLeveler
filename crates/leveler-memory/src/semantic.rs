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
//! Nothing here touches a `MemoryStore`. The extractor cannot write, supersede,
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
use crate::lifecycle::{CandidateOperation, MemoryAuthority, semantic_key_of};

/// Bounds of a single atomic fact. Long enough for a real sentence, short
/// enough that a multi-sentence blob cannot slip through as one memory.
const MAX_FACT_CHARS: usize = 240;
/// Bounds of the quoted user evidence.
const MAX_EVIDENCE_CHARS: usize = 300;
/// Bounds of the subject identity string.
const MAX_SUBJECT_CHARS: usize = 96;
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
    /// The value when the fact is an assignment (`Pro`, `required`, `forbidden`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub value: Option<String>,
    pub scope: CandidateScope,
    pub durability: CandidateDurability,
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
    /// `fact` read as several statements joined together.
    NotAtomic,
    /// `subject` was empty or too long to be a stable identity.
    InvalidSubject,
    /// Only `explicit_user` may auto-commit this round.
    NotExplicitUser { authority: MemoryAuthority },
    /// Only a durable statement may auto-commit.
    NotDurable { durability: CandidateDurability },
    /// Only project scope may auto-commit this round.
    NotProjectScope { scope: CandidateScope },
    /// The evidence span was empty or not found in the current user message.
    EvidenceNotFound,
    /// The candidate text looked like a credential.
    Sensitive,
    /// The model itself reported low confidence.
    LowConfidence { confidence: f32 },
}

impl CandidateRejection {
    /// A stable machine code for tracing.
    pub fn code(&self) -> &'static str {
        match self {
            Self::EmptyFact => "empty_fact",
            Self::FactTooLong { .. } => "fact_too_long",
            Self::NotAtomic => "not_atomic",
            Self::InvalidSubject => "invalid_subject",
            Self::NotExplicitUser { .. } => "not_explicit_user",
            Self::NotDurable { .. } => "not_durable",
            Self::NotProjectScope { .. } => "not_project_scope",
            Self::EvidenceNotFound => "evidence_not_found",
            Self::Sensitive => "sensitive",
            Self::LowConfidence { .. } => "low_confidence",
        }
    }

    /// A short human-readable reason for a debug log. Never shown in the
    /// normal UI — a skipped candidate is not an event.
    pub fn reason(&self) -> String {
        match self {
            Self::EmptyFact => "candidate fact was empty".to_string(),
            Self::FactTooLong { chars } => format!("candidate fact was {chars} chars"),
            Self::NotAtomic => "candidate read as more than one statement".to_string(),
            Self::InvalidSubject => "candidate subject was not a usable identity".to_string(),
            Self::NotExplicitUser { authority } => {
                format!("authority {} may not auto-commit", authority.as_str())
            }
            Self::NotDurable { durability } => format!("durability {durability:?} is not durable"),
            Self::NotProjectScope { scope } => format!("scope {scope:?} is not project"),
            Self::EvidenceNotFound => {
                "evidence span was not found in the current user message".to_string()
            }
            Self::Sensitive => "candidate looked like a secret".to_string(),
            Self::LowConfidence { confidence } => {
                format!("model confidence {confidence:.2} was below threshold")
            }
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
    if !is_atomic(fact) {
        return Err(CandidateRejection::NotAtomic);
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

    if let Some(confidence) = candidate.confidence
        && confidence < MIN_CONFIDENCE
    {
        return Err(CandidateRejection::LowConfidence { confidence });
    }

    if !evidence_matches(&candidate.evidence_span, user_message) {
        return Err(CandidateRejection::EvidenceNotFound);
    }

    if looks_like_secret(fact)
        || looks_like_secret(&candidate.evidence_span)
        || candidate.value.as_deref().is_some_and(looks_like_secret)
    {
        return Err(CandidateRejection::Sensitive);
    }

    let subject = canonical_subject(&candidate.subject);
    if subject.is_empty() || subject.chars().count() > MAX_SUBJECT_CHARS {
        return Err(CandidateRejection::InvalidSubject);
    }

    let key = semantic_key_of(&subject);
    let mut memory = MemoryCandidate::new(
        fact,
        fact,
        CandidateKind::Free,
        Some(key.clone()),
        CandidateSource::UserExplicit,
        vec![
            "explicit".to_string(),
            "decision".to_string(),
            "semantic".to_string(),
        ],
    )
    .map_err(|_| CandidateRejection::Sensitive)?;
    memory.semantic_key = Some(key);
    memory.authority = MemoryAuthority::ExplicitUser;
    memory.operation = match candidate.operation_hint {
        OperationHint::Update | OperationHint::Negate => CandidateOperation::Update,
        OperationHint::Create
        | OperationHint::Reaffirm
        | OperationHint::Temporary
        | OperationHint::Unknown => CandidateOperation::Create,
    };
    memory.evidence = Some(clip(&candidate.evidence_span, MAX_EVIDENCE_CHARS));
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

/// Fold to comparable characters: lowercase alphanumerics (Unicode-aware).
fn normalize_for_match(text: &str) -> String {
    text.chars()
        .filter(|c| c.is_alphanumeric())
        .flat_map(|c| c.to_lowercase())
        .collect()
}

/// A fact is atomic when it is one statement, not a concatenation of several.
/// Sentence terminators and list separators inside the fact are the tell: an
/// atomic fact is one clause and never needs them.
fn is_atomic(fact: &str) -> bool {
    const TERMINATORS: [char; 9] = ['。', '；', ';', '！', '!', '？', '?', '\n', '\r'];
    if fact.contains(TERMINATORS) {
        return false;
    }
    // Two or more commas read as an enumeration, which is a blob wherever the
    // individual items have different identities.
    let commas = fact.matches([',', '，']).count();
    commas < 2
}

/// Normalize a subject into the shared identity space used by the deterministic
/// fast path, so a fact stated once in canonical form and later in Chinese (or
/// vice versa) still maps to one memory.
///
/// This is identity normalization, not extraction: it is a small, explicit
/// alias table, and an unknown subject is kept verbatim so it can never
/// collide with a different fact by accident.
pub fn canonical_subject(raw: &str) -> String {
    let stripped = raw.trim();
    let stripped = stripped
        .strip_prefix("project.")
        .or_else(|| stripped.strip_prefix("project_"))
        .unwrap_or(stripped);
    let dotted = stripped.replace('.', "_");
    let compact: String = dotted
        .chars()
        .filter(|c| !c.is_whitespace())
        .collect::<String>()
        .to_lowercase();
    for (alias, canonical) in SUBJECT_ALIASES {
        if compact == *alias {
            return (*canonical).to_string();
        }
    }
    dotted
}

/// Alias groups keyed by the whitespace-free lowercase spelling. Each group
/// collapses on one canonical subject; the fast path's Chinese slot names and
/// the extractor's dotted identities land on the same key.
const SUBJECT_ALIASES: &[(&str, &str)] = &[
    ("default_model", "默认模型"),
    ("defaultmodel", "默认模型"),
    ("默认模型", "默认模型"),
    ("默认", "默认模型"),
    ("model", "默认模型"),
    ("windows_support", "windows支持"),
    ("windowssupport", "windows支持"),
    ("windows", "windows支持"),
    ("windows支持", "windows支持"),
    ("平台支持", "windows支持"),
    ("platform.windows.support", "windows支持"),
    ("protected_branch_push", "保护分支推送"),
    ("protectedbranchpush", "保护分支推送"),
    ("保护分支推送", "保护分支推送"),
    ("push_policy", "保护分支推送"),
    ("pushpolicy", "保护分支推送"),
    ("分支推送", "保护分支推送"),
    ("language", "编程语言"),
    ("language_standard", "编程语言"),
    ("languagestandard", "编程语言"),
    ("编程语言", "编程语言"),
    ("技术栈", "编程语言"),
];

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
            value: None,
            scope: CandidateScope::Project,
            durability: CandidateDurability::Durable,
            authority: MemoryAuthority::ExplicitUser,
            operation_hint: OperationHint::Create,
            evidence_span: evidence.to_string(),
            confidence: None,
        }
    }

    #[test]
    fn parses_a_bare_array_and_an_object_wrapper() {
        let json = r#"[{"fact":"默认模型是 Pro","subject":"project.default_model",
            "scope":"project","durability":"durable","authority":"explicit_user",
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
    fn parses_dotted_subject_inside_braces_in_strings() {
        let raw = r#"prefix {"candidates":[{"fact":"a } b","subject":"s","scope":"project","durability":"durable","authority":"explicit_user","operation_hint":"create","evidence_span":"x"}]} trailing"#;
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
    fn a_valid_candidate_becomes_an_explicit_user_memory() {
        let msg = "这个项目后面模型就 Pro 吧";
        let c = candidate("项目默认模型为 Pro", "project.default_model", "模型就 Pro");
        let memory = validate_semantic_candidate(&c, msg).unwrap();
        assert_eq!(memory.authority, MemoryAuthority::ExplicitUser);
        assert_eq!(memory.source, CandidateSource::UserExplicit);
        assert_eq!(
            memory.semantic_key.as_deref(),
            Some(semantic_key_of("默认模型").as_str())
        );
        assert_eq!(memory.evidence.as_deref(), Some("模型就 Pro"));
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
    fn secrets_and_blobs_are_refused() {
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

        let msg2 = "以后统一用 Rust，Windows 也必须支持，而且 protected branch 不要直接 push。";
        let mut blob = candidate(
            "项目以后用 Rust 且支持 Windows 且禁止 push",
            "language",
            "以后统一用 Rust",
        );
        blob.fact = "用 Rust，支持 Windows，禁止 push".to_string();
        assert_eq!(
            validate_semantic_candidate(&blob, msg2).unwrap_err(),
            CandidateRejection::NotAtomic
        );
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

    #[test]
    fn canonical_subject_unifies_fast_path_and_semantic_spellings() {
        for subject in ["project.default_model", "default_model", "默认模型", "默认"] {
            assert_eq!(
                semantic_key_of(&canonical_subject(subject)),
                semantic_key_of("默认模型"),
                "{subject}"
            );
        }
    }

    #[test]
    fn unknown_subjects_stay_distinct() {
        assert_ne!(
            semantic_key_of(&canonical_subject("project.release_probe_codename")),
            semantic_key_of(&canonical_subject("project.deploy_window"))
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

    #[test]
    fn duplicate_facts_for_one_subject_collapse() {
        let msg = "默认模型用 Pro";
        let batch = vec![
            candidate("默认模型是 Pro", "project.default_model", "默认模型用 Pro"),
            candidate("默认模型是 Pro", "默认模型", "默认模型用 Pro"),
        ];
        let validated = validate_semantic_candidates(&batch, msg);
        assert_eq!(validated.accepted.len(), 1);
    }
}
