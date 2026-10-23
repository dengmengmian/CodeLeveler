//! Structured records for the Markdown memory store.
//!
//! These types are deliberately small and deterministic. Nothing here infers a
//! subject, an equivalence, or an authority from free text.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_yaml::Mapping;
use uuid::Uuid;

use super::frontmatter::DocumentError;
use super::lifecycle::Admission;

pub const SCHEMA_VERSION: u32 = 2;
pub const MAX_ID_BYTES: usize = 96;
pub const MAX_TITLE_CHARS: usize = 200;
pub const MAX_SUBJECT_CHARS: usize = 96;
pub const MAX_VALUE_CHARS: usize = 512;
pub const MAX_TAG_CHARS: usize = 32;
pub const MAX_TAGS: usize = 16;
pub const MAX_EVIDENCE_SPAN_CHARS: usize = 512;
pub const MAX_BODY_BYTES: usize = 64 * 1024;
pub const MAX_FRONTMATTER_BYTES: usize = 32 * 1024;
pub const MAX_FILE_BYTES: usize = 128 * 1024;

/// A stable memory identifier. The file name and the identity are both derived
/// from this value; neither is derived from the body.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct MemoryId(String);

impl MemoryId {
    pub fn new() -> Self {
        Self(format!("mem_{}", Uuid::new_v4().simple()))
    }

    pub fn parse(value: impl Into<String>) -> Result<Self, RecordError> {
        let value = value.into();
        if value.is_empty() || value.len() > MAX_ID_BYTES {
            return Err(RecordError::InvalidId(value));
        }
        if !value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
        {
            return Err(RecordError::InvalidId(value));
        }
        Ok(Self(value))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn file_name(&self) -> String {
        format!("{}.md", self.0)
    }
}

impl std::fmt::Display for MemoryId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl Default for MemoryId {
    fn default() -> Self {
        Self::new()
    }
}

/// Operation identity for idempotent retries. It is supplied by the caller and
/// is never synthesized from a turn id by the store.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct OpId(String);

impl OpId {
    pub fn parse(value: impl Into<String>) -> Result<Self, RecordError> {
        let value = value.into();
        if value.is_empty() || value.len() > 160 {
            return Err(RecordError::InvalidOpId(value));
        }
        if !value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b':' | b'.'))
        {
            return Err(RecordError::InvalidOpId(value));
        }
        Ok(Self(value))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for OpId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// Who initiated a write. `User` is intentionally not constructible from a
/// model payload, CLI argument, or YAML field; this enum is not `Deserialize`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Caller {
    UserCommand { invocation: String },
    UserReview { invocation: String },
    Model { tool_call: String },
    Runtime { task: String },
    Migration { run: String },
    External,
}

impl Caller {
    pub fn is_user(&self) -> bool {
        matches!(self, Self::UserCommand { .. } | Self::UserReview { .. })
    }

    pub fn is_external(&self) -> bool {
        matches!(self, Self::External)
    }

    pub fn authority_for(&self, kind: MemoryKind, has_resolution: bool) -> MemoryAuthority {
        match self {
            Self::UserCommand { .. } => MemoryAuthority::UserExplicit,
            Self::UserReview { .. } => MemoryAuthority::UserApproved,
            Self::Model { .. } => MemoryAuthority::ModelInferred,
            Self::Runtime { .. } => match kind {
                MemoryKind::Experience if has_resolution => MemoryAuthority::RuntimeObserved,
                MemoryKind::Experience => MemoryAuthority::ToolObserved,
                MemoryKind::UserFact => MemoryAuthority::UserStated,
            },
            Self::Migration { .. } => MemoryAuthority::ModelInferred,
            Self::External => MemoryAuthority::ModelInferred,
        }
    }

    pub fn label(&self) -> &'static str {
        match self {
            Self::UserCommand { .. } => "user_command",
            Self::UserReview { .. } => "user_review",
            Self::Model { .. } => "model",
            Self::Runtime { .. } => "runtime",
            Self::Migration { .. } => "migration",
            Self::External => "external",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MemoryKind {
    UserFact,
    Experience,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MemoryScope {
    Global,
    Project,
    Local,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MemoryStatus {
    Active,
    Pending,
    Superseded,
    Deprecated,
    Expired,
}

impl MemoryStatus {
    pub fn is_active(self) -> bool {
        matches!(self, Self::Active)
    }

    pub fn is_current(self) -> bool {
        matches!(self, Self::Active | Self::Pending)
    }
}

/// Authority is derived from the write path, never from model-supplied JSON or
/// external frontmatter. The ordering is used only for conflict/ranking; it is
/// not a permission.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MemoryAuthority {
    ModelInferred,
    ToolObserved,
    RuntimeObserved,
    ProjectSource,
    UserStated,
    UserApproved,
    UserExplicit,
}

impl MemoryAuthority {
    pub fn is_user_decision(self) -> bool {
        matches!(self, Self::UserApproved | Self::UserExplicit)
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::ModelInferred => "model_inferred",
            Self::ToolObserved => "tool_observed",
            Self::RuntimeObserved => "runtime_observed",
            Self::ProjectSource => "project_source",
            Self::UserStated => "user_stated",
            Self::UserApproved => "user_approved",
            Self::UserExplicit => "user_explicit",
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StatementKind {
    #[default]
    Assertion,
    Correction,
    Rejection,
    Note,
}

impl StatementKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Assertion => "assertion",
            Self::Correction => "correction",
            Self::Rejection => "rejection",
            Self::Note => "note",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PinOwner {
    User,
    Model,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Operation {
    Create,
    Update,
    Supersede,
    Deprecate,
    Expire,
    Forget,
    Pin,
    Unpin,
    Review,
}

impl Operation {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Create => "created",
            Self::Update => "updated",
            Self::Supersede => "superseded",
            Self::Deprecate => "deprecated",
            Self::Expire => "expired",
            Self::Forget => "forgotten",
            Self::Pin => "pinned",
            Self::Unpin => "unpinned",
            Self::Review => "reviewed",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AdmissionPayload {
    pub id: MemoryId,
    pub revision: u64,
    pub operation: Operation,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Relation {
    None,
    Update { target: MemoryId },
    Supersede { targets: Vec<MemoryId> },
    Replaces { target: MemoryId },
}

/// Content-addressed compare-and-swap token. The digest is the SHA-256 of the
/// exact canonical bytes on disk, not of the parsed structure.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Cas {
    pub revision: u64,
    pub digest: String,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct MemoryIntent {
    pub kind: MemoryKind,
    pub scope: MemoryScope,
    pub subject: Option<String>,
    pub subject_key: Option<String>,
    pub value: Option<String>,
    pub title: String,
    pub body: String,
    pub tags: Vec<String>,
    pub statement: StatementKind,
    pub relation: Relation,
    pub expires_at: Option<DateTime<Utc>>,
    pub evidence: Vec<SourceRef>,
    pub experience: Option<Experience>,
    pub proposed_authority: MemoryAuthority,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SourceRef {
    #[serde(rename = "type")]
    pub kind: SourceRefKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub turn_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub task_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pending_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub evidence_span: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message_digest: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub span_range: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub file_sha256: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub legacy_store: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub legacy_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub legacy_sha256: Option<String>,
    #[serde(flatten, default, skip_serializing_if = "Mapping::is_empty")]
    pub extra: Mapping,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SourceRefKind {
    UserCommand,
    UserTurn,
    UserReview,
    ToolCall,
    RuntimeTask,
    Migration,
    ExternalEdit,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Proposal {
    pub operation: Operation,
    #[serde(default)]
    pub target_ids: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expected_revision: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expected_digest: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub proposed_authority: Option<MemoryAuthority>,
    #[serde(default)]
    pub reason: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Experience {
    pub category: String,
    pub applies_when: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub invalid_if: Option<String>,
    #[serde(default)]
    pub environment: Mapping,
    #[serde(default)]
    pub evidence: Vec<ExperienceEvidence>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExperienceEvidence {
    pub kind: String,
    pub reference: String,
}

impl Experience {
    pub fn has_resolution(&self) -> bool {
        let mut seen_failure = false;
        for evidence in &self.evidence {
            match evidence.kind.as_str() {
                "failure" => seen_failure = true,
                "resolution" if seen_failure => return true,
                _ => {}
            }
        }
        false
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Frontmatter {
    pub schema: u32,
    pub id: MemoryId,
    pub kind: MemoryKind,
    pub scope: MemoryScope,
    pub status: MemoryStatus,
    pub revision: u64,
    pub authority: MemoryAuthority,
    pub title: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subject: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subject_key: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub value: Option<String>,
    #[serde(default)]
    pub statement: StatementKind,
    #[serde(default)]
    pub tags: Vec<String>,
    #[serde(default)]
    pub pinned: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pinned_by: Option<PinOwner>,
    #[serde(default)]
    pub sensitive: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<DateTime<Utc>>,
    #[serde(default)]
    pub supersedes: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub superseded_by: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub proposal: Option<Proposal>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub experience: Option<Experience>,
    #[serde(default)]
    pub source_refs: Vec<SourceRef>,
    #[serde(with = "rfc3339")]
    pub created_at: DateTime<Utc>,
    #[serde(with = "rfc3339")]
    pub updated_at: DateTime<Utc>,
    #[serde(flatten, default, skip_serializing_if = "Mapping::is_empty")]
    pub extra: Mapping,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MemoryRecord {
    pub frontmatter: Frontmatter,
    #[serde(default)]
    pub body: String,
}

impl MemoryRecord {
    pub fn id(&self) -> &MemoryId {
        &self.frontmatter.id
    }

    pub fn status(&self) -> MemoryStatus {
        self.frontmatter.status
    }

    pub fn revision(&self) -> u64 {
        self.frontmatter.revision
    }

    pub fn subject_key(&self) -> Option<&str> {
        self.frontmatter.subject_key.as_deref()
    }

    pub fn is_active(&self) -> bool {
        self.frontmatter.status.is_active()
    }

    pub fn validate(&self) -> Result<(), DocumentError> {
        self.frontmatter.validate()?;
        if self.body.len() > MAX_BODY_BYTES {
            return Err(DocumentError::BodyTooLarge);
        }
        if let Some(field) = self.secret_field() {
            return Err(DocumentError::SecretDetected(field));
        }
        Ok(())
    }

    /// Returns the first field whose value trips the credential scan. The
    /// returned name is safe to log; the value is not.
    pub fn secret_field(&self) -> Option<&'static str> {
        let fm = &self.frontmatter;
        if crate::looks_like_secret(&fm.title) {
            return Some("title");
        }
        if fm.subject.as_deref().is_some_and(crate::looks_like_secret) {
            return Some("subject");
        }
        if fm
            .subject_key
            .as_deref()
            .is_some_and(crate::looks_like_secret)
        {
            return Some("subject_key");
        }
        if fm.value.as_deref().is_some_and(crate::looks_like_secret) {
            return Some("value");
        }
        if fm.tags.iter().any(|tag| crate::looks_like_secret(tag)) {
            return Some("tags");
        }
        if crate::looks_like_secret(&self.body) {
            return Some("body");
        }
        for source in &fm.source_refs {
            if source
                .evidence_span
                .as_deref()
                .is_some_and(crate::looks_like_secret)
            {
                return Some("source_refs.evidence_span");
            }
            if source
                .message_digest
                .as_deref()
                .is_some_and(crate::looks_like_secret)
            {
                return Some("source_refs.message_digest");
            }
            if mapping_has_secret(&source.extra) {
                return Some("source_refs");
            }
        }
        if let Some(proposal) = &fm.proposal {
            if crate::looks_like_secret(&proposal.reason)
                || proposal
                    .target_ids
                    .iter()
                    .any(|target| crate::looks_like_secret(target))
            {
                return Some("proposal");
            }
        }
        if let Some(experience) = &fm.experience {
            if crate::looks_like_secret(&experience.category)
                || crate::looks_like_secret(&experience.applies_when)
                || experience
                    .invalid_if
                    .as_deref()
                    .is_some_and(crate::looks_like_secret)
                || experience
                    .evidence
                    .iter()
                    .any(|e| crate::looks_like_secret(&e.reference))
                || mapping_has_secret(&experience.environment)
            {
                return Some("experience");
            }
        }
        if mapping_has_secret(&fm.extra) {
            return Some("extra");
        }
        None
    }
}

impl Frontmatter {
    pub fn validate(&self) -> Result<(), DocumentError> {
        if self.schema != SCHEMA_VERSION {
            return Err(DocumentError::UnsupportedSchema(self.schema));
        }
        if self.revision == 0 {
            return Err(DocumentError::InvalidRevision);
        }
        let title_len = self.title.chars().count();
        if title_len == 0 || title_len > MAX_TITLE_CHARS {
            return Err(DocumentError::InvalidTitle);
        }
        if self
            .subject
            .as_deref()
            .is_some_and(|s| s.chars().count() > MAX_SUBJECT_CHARS)
        {
            return Err(DocumentError::InvalidSubject);
        }
        if self
            .value
            .as_deref()
            .is_some_and(|v| v.chars().count() > MAX_VALUE_CHARS)
        {
            return Err(DocumentError::InvalidValue);
        }
        if self.tags.len() > MAX_TAGS
            || self
                .tags
                .iter()
                .any(|tag| tag.is_empty() || tag.chars().count() > MAX_TAG_CHARS)
        {
            return Err(DocumentError::InvalidTags);
        }
        if self.kind == MemoryKind::UserFact && self.subject_key.is_none() && !self.legacy_keyless()
        {
            return Err(DocumentError::MissingSubjectKey);
        }
        if self
            .subject_key
            .as_deref()
            .is_some_and(|key| key.is_empty() || key.chars().count() > MAX_SUBJECT_CHARS)
        {
            return Err(DocumentError::InvalidSubject);
        }
        if self.kind == MemoryKind::Experience && self.experience.is_none() {
            return Err(DocumentError::MissingExperience);
        }
        if self.kind == MemoryKind::UserFact && self.experience.is_some() {
            return Err(DocumentError::UnexpectedExperience);
        }
        if self.status == MemoryStatus::Pending && self.proposal.is_none() {
            return Err(DocumentError::MissingProposal);
        }
        if self.status == MemoryStatus::Superseded && self.superseded_by.is_none() {
            return Err(DocumentError::MissingSupersededBy);
        }
        if self.status != MemoryStatus::Superseded && self.superseded_by.is_some() {
            return Err(DocumentError::UnexpectedSupersededBy);
        }
        if self.status == MemoryStatus::Active
            && self
                .expires_at
                .is_some_and(|expires| expires <= self.updated_at)
        {
            return Err(DocumentError::ExpiredButActive);
        }
        if self.pinned && self.pinned_by.is_none() {
            return Err(DocumentError::MissingPinOwner);
        }
        if !self.pinned && self.pinned_by.is_some() {
            return Err(DocumentError::UnexpectedPinOwner);
        }
        for source in &self.source_refs {
            if let Some(span) = &source.evidence_span {
                if span.chars().count() > MAX_EVIDENCE_SPAN_CHARS {
                    return Err(DocumentError::EvidenceTooLong);
                }
            }
        }
        Ok(())
    }

    fn legacy_keyless(&self) -> bool {
        self.extra
            .get(serde_yaml::Value::String("legacy".to_string()))
            .and_then(|legacy| legacy.as_mapping())
            .and_then(|legacy| legacy.get(serde_yaml::Value::String("keyless".to_string())))
            .and_then(serde_yaml::Value::as_bool)
            .unwrap_or(false)
    }
}

fn value_has_secret(value: &serde_yaml::Value) -> bool {
    match value {
        serde_yaml::Value::String(text) => crate::looks_like_secret(text),
        serde_yaml::Value::Sequence(items) => items.iter().any(value_has_secret),
        serde_yaml::Value::Mapping(map) => mapping_has_secret(map),
        _ => false,
    }
}

fn mapping_has_secret(map: &serde_yaml::Mapping) -> bool {
    map.iter()
        .any(|(key, value)| value_has_secret(key) || value_has_secret(value))
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IntentRecord {
    pub op_id: OpId,
    pub request_digest: String,
    pub admission: Option<Admission>,
    pub created_at: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Tombstone {
    pub id: MemoryId,
    pub kind: MemoryKind,
    pub scope: MemoryScope,
    pub subject_key_sha256: Option<String>,
    pub last_revision: u64,
    pub content_sha256: String,
    pub forgotten_at: String,
}

pub mod rfc3339 {
    use chrono::{DateTime, SecondsFormat, Utc};
    use serde::{Deserialize, Deserializer, Serializer};

    pub fn serialize<S>(value: &DateTime<Utc>, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(&value.to_rfc3339_opts(SecondsFormat::Secs, true))
    }

    pub fn deserialize<'de, D>(deserializer: D) -> Result<DateTime<Utc>, D::Error>
    where
        D: Deserializer<'de>,
    {
        let text = String::deserialize(deserializer)?;
        DateTime::parse_from_rfc3339(&text)
            .map(|value| value.with_timezone(&Utc))
            .map_err(serde::de::Error::custom)
    }
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum RecordError {
    #[error("invalid memory id: {0}")]
    InvalidId(String),
    #[error("invalid operation id: {0}")]
    InvalidOpId(String),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_reject_path_characters() {
        assert!(MemoryId::parse("../escape").is_err());
        assert!(MemoryId::parse("mem/child").is_err());
        assert!(MemoryId::parse("mem_ok-1").is_ok());
    }

    #[test]
    fn op_ids_do_not_accept_paths() {
        assert!(OpId::parse("../../op").is_err());
        assert!(OpId::parse("call_01J").is_ok());
    }

    #[test]
    fn caller_is_not_deserializable() {
        // This is a compile-time contract test: `Caller` has no Deserialize
        // implementation. The assertions below exercise the trusted mapping.
        assert!(
            Caller::UserCommand {
                invocation: "inv-1".into()
            }
            .is_user()
        );
        assert!(
            !Caller::Model {
                tool_call: "call-1".into()
            }
            .is_user()
        );
    }
}
