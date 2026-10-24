//! Pure memory-candidate extraction (no I/O except optional path reads).
//!
//! System-proposed candidates still go through the memory lifecycle (user
//! consent for a higher-authority replacement) before becoming active
//! entries. Extractors never write durable memory themselves.

use serde::{Deserialize, Serialize};

use crate::util::{now_rfc3339, short_hash, slugify};
use crate::validation::ValidationError;

/// How a candidate was produced.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CandidateSource {
    /// An explicit user statement of a durable rule ("以后默认模型固定为 X").
    /// This records that the fact came from the user's own words and therefore
    /// carries explicit-user authority; it does NOT mean the user consented to
    /// persist it. Except for an explicit direct-memory command, it must still
    /// remain pending until the user accepts it.
    /// (Historically also the label on a "记住：…" candidate; kept so old
    /// `pending/` files still deserialize.)
    UserExplicit,
    /// LEGACY spelling of [`Self::SystemInferred`], kept for the same reason.
    SystemPropose,
    /// Inferred from how the user talked ("我通常希望输出短一点"). A guess
    /// about a preference, so it needs consent before it becomes memory.
    SystemInferred,
    /// The model proposed it with the `remember` tool. Distinct from a system
    /// guess because the user is approving the MODEL's judgement, and they
    /// deserve to know which one they are looking at.
    AgentProposed,
    /// The user asked to restore a historical entry. It carries explicit-user
    /// authority because the command is the person's own decision, while its
    /// provenance keeps the restore path auditable.
    UserRestore,
    /// A person accepted a pending candidate. The fact now carries
    /// explicit-user authority, and the original proposal source remains in
    /// the entry's provenance evidence.
    UserAccepted,
    /// The runtime observed a failure and its resolution in a completed task
    /// and extracted an experience candidate from those structured facts.
    /// Distinct from [`Self::AgentProposed`] because the claim is bound to
    /// evidence the runtime recorded, not to a model's own judgement.
    RuntimeObserved,
}

impl CandidateSource {
    /// Stable label for user-facing projections.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::UserExplicit => "user_explicit",
            Self::SystemPropose => "system_propose",
            Self::SystemInferred => "system_inferred",
            Self::AgentProposed => "agent_proposed",
            Self::UserRestore => "user_restore",
            Self::UserAccepted => "user_accepted",
            Self::RuntimeObserved => "runtime_observed",
        }
    }

    /// Whether this is a value only old data carries.
    pub fn is_legacy(self) -> bool {
        matches!(self, Self::UserExplicit | Self::SystemPropose)
    }
}

/// Stable category for structured keys and suppress logic.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CandidateKind {
    Preference,
    PackageManager,
    Free,
}

/// A pending memory proposal. Not durable project memory until accepted.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MemoryCandidate {
    pub id: String,
    pub title: String,
    pub body: String,
    #[serde(default)]
    pub tags: Vec<String>,
    pub kind: CandidateKind,
    /// Stable key for dedup / suppress (e.g. `package_manager`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub key: Option<String>,
    /// Content fingerprint for suppress-after-reject.
    pub fingerprint: String,
    pub source: CandidateSource,
    pub created_at: String,
    /// Semantic identity of the fact this candidate states (a normalized
    /// subject). Two statements about the same subject share it, so a later
    /// one can supersede an earlier one instead of accumulating beside it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub semantic_key: Option<String>,
    /// A structured relation suggested by the model: the subject of the
    /// active memory this candidate restates, replaces, or contradicts.
    /// Normalized like every subject. The runtime validates that the target
    /// exists; it never infers a relation from wording similarity.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub related_key: Option<String>,
    /// The exact active memory this candidate replaces, when the model named
    /// an id from the supplied identity catalog. This reaches entries that
    /// have no semantic key (legacy direct notes). The runtime resolves the
    /// id against active memory; an unresolvable id is parked, never guessed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub replaces_id: Option<String>,
    /// The value the fact assigns, when it is an assignment. Compared exactly
    /// as a structured field, so "same subject, same value" never depends on
    /// comparing two sentences.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub value: Option<String>,
    /// How much weight this candidate carries when it conflicts with an
    /// existing memory. Defaults to the weakest class for old `pending/` files.
    #[serde(default)]
    pub authority: MemoryAuthority,
    /// Whether the candidate states a new fact or corrects an existing one.
    /// A hint for the commit decision; the decision re-derives it from the store.
    #[serde(default)]
    pub operation: CandidateOperation,
    /// Optional expiry (RFC 3339). Present means the fact is time-bound and
    /// must not survive past it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<String>,
    /// The user's own words this candidate was drawn from. Present for the
    /// semantic path, absent for older candidates. Carried into provenance so
    /// an autonomous write can always be traced back to the sentence that
    /// authorized it. Never a secret (checked at construction).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub evidence: Option<String>,
    /// The trusted session the evidence span was read from. Set by the runtime
    /// that read the turn — the extractor never sees a session id — so a stored
    /// fact points at a real message a person can still open.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub evidence_session: Option<String>,
    /// The trusted turn the evidence span was read from, for the same reason.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub evidence_turn: Option<String>,
    /// The structured evidence contract for an agent experience. Present only
    /// for [`CandidateSource::RuntimeObserved`] (or a user-accepted/restored
    /// experience); absent everywhere else.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub experience: Option<crate::ExperienceRecord>,
}

/// Stable fingerprint used for suppress-after-reject and dedup.
fn fingerprint_of(kind: CandidateKind, key: Option<&str>, title: &str, body: &str) -> String {
    let kind_s = match kind {
        CandidateKind::Preference => "preference",
        CandidateKind::PackageManager => "package_manager",
        CandidateKind::Free => "free",
    };
    short_hash(&format!("{kind_s}|{}|{title}|{body}", key.unwrap_or("")))
}

impl MemoryCandidate {
    /// Build a candidate with slug id and fingerprint derived from kind/key/body.
    pub fn new(
        title: impl Into<String>,
        body: impl Into<String>,
        kind: CandidateKind,
        key: Option<String>,
        source: CandidateSource,
        tags: Vec<String>,
    ) -> Result<Self, ValidationError> {
        let title = title.into().trim().to_string();
        let body = body.into().trim().to_string();
        if title.is_empty() || body.is_empty() {
            return Err(ValidationError::Invalid(
                "candidate title and body are required".into(),
            ));
        }
        if looks_like_secret(&title)
            || looks_like_secret(&body)
            || tags.iter().any(|tag| looks_like_secret(tag))
        {
            return Err(ValidationError::Invalid(
                "refusing candidate that looks like a secret".into(),
            ));
        }
        let fingerprint = fingerprint_of(kind, key.as_deref(), &title, &body);
        let id_base = key
            .as_deref()
            .map(|k| format!("cand-{k}"))
            .unwrap_or_else(|| format!("cand-{}", slugify(&title)));
        let id = slugify(&id_base);
        Ok(Self {
            id,
            title,
            body,
            tags,
            kind,
            semantic_key: key.clone(),
            key,
            related_key: None,
            replaces_id: None,
            value: None,
            fingerprint,
            source,
            created_at: now_rfc3339(),
            authority: MemoryAuthority::ModelInference,
            operation: CandidateOperation::Create,
            expires_at: None,
            evidence: None,
            evidence_session: None,
            evidence_turn: None,
            experience: None,
        })
    }
}

/// Heuristic: refuse API keys / tokens in candidate text.
pub fn looks_like_secret(text: &str) -> bool {
    let lower = text.to_ascii_lowercase();
    const MARKERS: &[&str] = &[
        "api_key",
        "apikey",
        "api key",
        "access_key",
        "access key",
        "secret_key",
        "secret key",
        "private_key",
        "private key",
        "client_secret",
        "client secret",
        "-----begin",
        "sk-",
        "ghp_",
        "gho_",
        "xoxb-",
        "bearer ",
        "authorization: ",
    ];
    if MARKERS.iter().any(|m| lower.contains(m)) {
        return true;
    }
    if contains_credential_assignment(text) || contains_provider_token(text) {
        return true;
    }
    // A contiguous padded base64 blob, rather than 40 unrelated alphanumeric
    // characters elsewhere in a sentence that also happens to contain `=`.
    text.split(|c: char| c.is_whitespace() || matches!(c, '"' | '\'' | '`'))
        .any(|part| {
            part.len() >= 40
                && part.contains('=')
                && part
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '/' | '='))
        })
}

/// Credential names are often more stable than their values. Recognise a
/// conventional assignment without treating ordinary prose about tokens or
/// keys as secret material.
fn contains_credential_assignment(text: &str) -> bool {
    for separator in ['=', ':', '：', '是', '为'] {
        for (left, right) in text.match_indices(separator).map(|(index, _)| {
            let after = index + separator.len_utf8();
            (&text[..index], &text[after..])
        }) {
            let value = right.trim();
            if value.chars().count() < 8 {
                continue;
            }
            let label = left
                .split_whitespace()
                .next_back()
                .unwrap_or("")
                .trim_matches(|c: char| !c.is_alphanumeric() && c != '_')
                .to_ascii_lowercase();
            const LABEL_SUFFIXES: &[&str] = &[
                "api_key",
                "apikey",
                "access_key",
                "secret_key",
                "private_key",
                "client_secret",
                "token",
                "password",
                "passwd",
                "credential",
                "密钥",
                "令牌",
                "密码",
                "凭证",
            ];
            if LABEL_SUFFIXES.iter().any(|suffix| label.ends_with(suffix)) {
                return true;
            }
        }
    }
    false
}

/// Well-known token prefixes whose payloads are opaque bearer credentials.
/// A minimum length keeps documentation that merely names a prefix recallable.
fn contains_provider_token(text: &str) -> bool {
    text.split(|c: char| {
        c.is_whitespace()
            || matches!(
                c,
                '"' | '\'' | '`' | '=' | ':' | '：' | ',' | '，' | ';' | '；'
            )
    })
    .any(|token| {
        let lower = token.to_ascii_lowercase();
        (token.len() >= 16
            && (token.starts_with("AKIA")
                || token.starts_with("AKLT")
                || token.starts_with("AIza")))
            || (token.len() >= 20
                && ["hf_", "glpat-", "github_pat_", "npm_", "pypi-"]
                    .iter()
                    .any(|prefix| lower.starts_with(prefix)))
            || (token.len() >= 40 && token.starts_with("eyJ") && token.matches('.').count() == 2)
    })
}

/// The body of a message that is ONLY a memory command, or `None`.
///
/// This is the parse behind a direct write, so it is deliberately narrow. It
/// recognises a commanding prefix at the very start (`记住：` / `请记住：` /
/// `remember:` / `please remember:`) and nothing else — no "always use X", no
/// "以后都…", because those are how people talk, not how they issue a command,
/// and a parser that rewrites long-term state must not guess.
///
/// Refuses when the message is negated, quotes the phrase, or wraps it in
/// code. Where it cannot be sure, it returns `None` and the message stays an
/// ordinary turn: failing to save is recoverable, while swallowing someone's
/// real task is not. The body after the prefix is stored verbatim: whether it
/// contains a second task is a question about meaning, and that belongs to the
/// model, not to a connector list here.
pub fn parse_direct_memory_command(text: &str) -> Option<String> {
    let t = text.trim();
    if t.is_empty() || looks_like_secret(t) {
        return None;
    }
    // Code fences or inline code anywhere: the phrase is being shown, not said.
    if t.contains("```") || t.contains('`') {
        return None;
    }
    // A quoted form is being discussed ("解释一下“记住：X”的含义").
    if t.contains('“') || t.contains('"') || t.contains('”') || t.contains('\'') {
        return None;
    }
    let body = strip_command_prefix(t)?;
    let body = body.trim();
    if body.chars().count() < 2 || body.chars().count() > 400 || looks_like_secret(body) {
        return None;
    }
    Some(body.to_string())
}

/// Commanding prefixes only, anchored at the first character. A negation
/// (`不要记住…`, `别记住…`) never matches because the prefix is not at the
/// start, which is the whole point of anchoring.
fn strip_command_prefix(text: &str) -> Option<&str> {
    for prefix in ["请记住：", "请记住:", "记住：", "记住:"] {
        if let Some(rest) = text.strip_prefix(prefix) {
            return Some(rest);
        }
    }
    let lower = text.to_ascii_lowercase();
    for prefix in ["please remember:", "remember:"] {
        if lower.starts_with(prefix) {
            return Some(&text[prefix.len()..]);
        }
    }
    None
}

/// The title a direct user write gets, derived deterministically from the body.
///
/// One implementation so TUI, Web and CLI cannot drift into three conventions,
/// and never model-generated — a title is not worth a round trip, and a model
/// naming the user's own note would be a second voice in their memory.
///
/// First non-empty line, cut at the first sentence end, capped at 48 chars.
pub fn title_from_body(body: &str) -> String {
    let line = body
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .unwrap_or("")
        .to_string();
    // Sentence enders in both scripts; whichever comes first wins.
    let cut = ['。', '！', '？', '；', '.', '!', '?', ';']
        .iter()
        .filter_map(|c| line.find(*c))
        .min();
    let sentence = match cut {
        Some(at) => &line[..at],
        None => line.as_str(),
    };
    let short: String = sentence.trim().chars().take(48).collect();
    if short.is_empty() {
        "记忆".to_string()
    } else {
        short
    }
}

/// How much weight a candidate carries when it conflicts with existing memory.
///
/// This is a property of the SOURCE, not a promise about the fact: the model's
/// own inference is the weakest class and can never replace an explicit user
/// decision on its own.
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

/// What the producer says it is doing to the identity it named.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CandidateOperation {
    #[default]
    Create,
    Update,
    Negate,
}

/// Normalize a subject into a stable identity. Deterministic, not semantic:
/// it folds spelling only — case, whitespace and separator characters. It
/// NEVER strips a trailing word: `billing_model` and `billing_setting` are two
/// different facts, and an identity that collapses them makes one silently
/// overwrite the other.
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
        format!("k-{}", short_hash(&normalized))
    }
}

pub fn is_canonical_key(value: &str) -> bool {
    value.starts_with("k-")
        && value.len() > 2
        && value
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

#[cfg(test)]
mod tests {
    // ---- strict command vs soft signal ----

    /// A message that is only a memory command writes directly. The user
    /// issuing the command IS the authorization; asking them to approve it a
    /// second time is what made the feature feel broken.
    #[test]
    fn a_strict_command_is_a_direct_write() {
        for input in [
            "记住：以后提交前先运行 pnpm lint",
            "记住: 以后提交前先运行 pnpm lint",
            "请记住：TUI 默认保持紧凑",
            "remember: never automatically commit",
            "Please remember: prefer concise terminal output",
        ] {
            let body = parse_direct_memory_command(input)
                .unwrap_or_else(|| panic!("must be a direct write: {input}"));
            assert!(!body.is_empty());
            assert!(!body.starts_with('：') && !body.starts_with(':'), "{body}");
        }
    }

    /// Negated, quoted, or code-wrapped forms must NOT write. Guessing wrong
    /// here either rewrites someone's long-term state or eats the work they
    /// actually asked for.
    #[test]
    fn ambiguous_forms_never_write_directly() {
        for input in [
            // negation
            "不要记住这个",
            "别记住：使用 npm",
            // quoting / discussing the phrase
            "解释一下“记住：使用 npm”是什么意思",
            "解释 \"remember: x\" 的含义",
            // code
            "把源码里的 `remember: foo` 改成 `remember: bar`",
            "代码中包含字符串 `记住：foo`",
            // not a command at all
            "我通常希望输出短一点",
            "always use pnpm",
            "以后都用 pnpm",
        ] {
            assert!(
                parse_direct_memory_command(input).is_none(),
                "must not write directly: {input}"
            );
        }
    }

    /// The body after the prefix is stored verbatim. Deciding that part of it
    /// is a second task is a judgment about meaning, so the parser does not
    /// make it; the user's command is consent for the text they wrote.
    #[test]
    fn a_command_body_is_stored_without_parsing_a_second_task() {
        let body = parse_direct_memory_command("记住：输出要简洁，然后修复 tests/a.rs")
            .expect("the commanding prefix is a direct write");
        assert_eq!(body, "输出要简洁，然后修复 tests/a.rs");
    }

    /// Only a commanding prefix is a direct write. Plain natural language is
    /// not classified here at all; the model extractor owns meaning.
    #[test]
    fn only_a_command_prefix_is_a_direct_write() {
        assert!(parse_direct_memory_command("记住：以后提交前先运行 pnpm lint").is_some());
        assert!(parse_direct_memory_command("我通常希望提交前先运行 pnpm lint").is_none());
        assert!(parse_direct_memory_command("always use pnpm").is_none());
    }

    /// A credential is refused on the direct path, before it can reach a file.
    #[test]
    fn a_direct_write_does_not_accept_a_secret() {
        let input = "记住：OPENAI_API_KEY=sk-live-abcdefghijklmnopqrstuvwxyz0123456789";
        assert!(parse_direct_memory_command(input).is_none());
    }

    /// An agent proposal must be distinguishable from a system guess: the user
    /// is approving the model's judgement, and should be told so.
    #[test]
    fn agent_and_system_sources_are_distinct_and_legacy_is_marked() {
        assert_ne!(
            CandidateSource::AgentProposed,
            CandidateSource::SystemInferred
        );
        assert!(!CandidateSource::AgentProposed.is_legacy());
        assert!(!CandidateSource::SystemInferred.is_legacy());
        assert!(CandidateSource::UserExplicit.is_legacy());
        assert!(CandidateSource::SystemPropose.is_legacy());
    }

    /// Old `pending/` files still load.
    #[test]
    fn a_canonical_key_is_its_own_identity() {
        // The extractor receives stored identities as subjects to reuse, so
        // folding must be idempotent: a key already in the namespace is its
        // own identity, never `k-k-...`.
        let key = semantic_key_of("default_model");
        assert!(is_canonical_key(&key), "{key}");
        assert_eq!(semantic_key_of(&key), key);
        // Case and separators fold; whitespace is dropped rather than turned
        // into a separator, so `Default Model` is not `default_model`.
        assert_eq!(semantic_key_of("DEFAULT_MODEL"), key);
        assert_eq!(semantic_key_of("default.model"), key);
        assert_eq!(semantic_key_of(" default_model "), key);
        assert!(!is_canonical_key("default model"));
    }

    #[test]
    fn legacy_sources_still_deserialize() {
        for wire in ["\"user_explicit\"", "\"system_propose\""] {
            let parsed: CandidateSource = serde_json::from_str(wire).expect(wire);
            assert!(parsed.is_legacy(), "{wire}");
        }
        let agent: CandidateSource = serde_json::from_str("\"agent_proposed\"").unwrap();
        assert_eq!(agent, CandidateSource::AgentProposed);
    }

    /// The title a direct write gets is derived, bounded, and never empty.
    #[test]
    fn a_derived_title_is_the_first_sentence_and_bounded() {
        assert_eq!(
            title_from_body("以后提交前先运行 pnpm lint。还有别的话。"),
            "以后提交前先运行 pnpm lint"
        );
        assert_eq!(title_from_body("first line\nsecond line"), "first line");
        assert_eq!(title_from_body("   "), "记忆");
        assert!(title_from_body(&"字".repeat(200)).chars().count() <= 48);
    }

    use super::*;

    #[test]
    fn explicit_chinese_remember_colon_is_a_direct_write() {
        let body = parse_direct_memory_command("记住：用 pnpm").expect("direct");
        assert!(body.contains("pnpm"), "{body}");
    }

    #[test]
    fn explicit_english_remember_is_a_direct_write() {
        let body =
            parse_direct_memory_command("remember: always use WorkspaceWrite").expect("direct");
        assert!(body.contains("WorkspaceWrite"), "{body}");
    }

    #[test]
    fn secrets_rejected() {
        assert!(parse_direct_memory_command("记住：api_key=sk-abc123secret").is_none());
        assert!(
            MemoryCandidate::new(
                "key",
                "Authorization: Bearer supersecrettokenvalue",
                CandidateKind::Free,
                None,
                CandidateSource::UserExplicit,
                vec![],
            )
            .is_err()
        );
    }

    /// Credentials are not limited to OpenAI/GitHub prefixes. Natural-language
    /// labels, conventional secret assignments and common provider token shapes
    /// must all stop at the single memory write boundary.
    #[test]
    fn common_credential_forms_are_sensitive() {
        for text in [
            "API key is demo-provider-value-1234567890",
            "部署密钥是 demo-provider-value-1234567890",
            "SERVICE_TOKEN=opaque-provider-value-1234567890",
            "DATABASE_PASSWORD: correct-horse-battery-staple",
            "AKIAIOSFODNN7EXAMPLE",
            "AKLTDEMO1234567890EXAMPLE",
            "AIzaSyD-demoGoogleApiKeyValue1234567890",
            "hf_demoModelTokenValue1234567890",
            "glpat-demoGitLabTokenValue1234567890",
            "github_pat_demoGitHubTokenValue1234567890",
            "npm_demoRegistryTokenValue1234567890",
            "pypi-AgEIdemoPackageTokenValue1234567890",
            "eyJhbGciOiJIUzI1NiJ9.eyJzdWIiOiJkZW1vIn0.signature",
        ] {
            assert!(
                looks_like_secret(text),
                "credential was not detected: {text}"
            );
            assert!(
                MemoryCandidate::new(
                    "project credential",
                    text,
                    CandidateKind::Free,
                    None,
                    CandidateSource::UserExplicit,
                    vec![],
                )
                .is_err(),
                "credential reached the memory write boundary: {text}"
            );
        }
    }

    #[test]
    fn ordinary_key_and_token_discussion_is_not_a_credential() {
        for text in [
            "token budget = 1000",
            "monkey=value",
            "部署密钥保管人：Zephyr-Q7",
            "document the hf_ prefix used in examples",
        ] {
            assert!(!looks_like_secret(text), "ordinary text was hidden: {text}");
        }
    }
}
