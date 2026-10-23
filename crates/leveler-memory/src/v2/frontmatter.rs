//! Canonical Markdown + YAML frontmatter encoding.
//!
//! The encoding is intentionally strict. A file we cannot parse
//! deterministically is not silently repaired; callers surface it as an
//! external conflict instead.

use std::collections::HashSet;

use sha2::{Digest, Sha256};

use super::record::{
    MAX_FILE_BYTES, MAX_FRONTMATTER_BYTES, MAX_ID_BYTES, MemoryRecord, SCHEMA_VERSION,
};

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum DocumentError {
    #[error("empty document")]
    Empty,
    #[error("document is not valid UTF-8")]
    NotUtf8,
    #[error("UTF-8 byte order mark is not accepted")]
    ByteOrderMark,
    #[error("document is larger than {MAX_FILE_BYTES} bytes")]
    DocumentTooLarge,
    #[error("frontmatter is larger than {MAX_FRONTMATTER_BYTES} bytes")]
    FrontmatterTooLarge,
    #[error("document does not start with a frontmatter delimiter")]
    MissingFrontmatter,
    #[error("frontmatter is not terminated")]
    UnterminatedFrontmatter,
    #[error("unsupported frontmatter schema: {0}")]
    UnsupportedSchema(u32),
    #[error("frontmatter must be a mapping")]
    NotMapping,
    #[error("duplicate frontmatter key: {0}")]
    DuplicateKey(String),
    #[error("frontmatter contains an alias or anchor")]
    AliasOrAnchor,
    #[error("frontmatter uses an unsupported top-level form")]
    UnsupportedTopLevelForm,
    #[error("frontmatter parse error: {0}")]
    Yaml(String),
    #[error("invalid memory id")]
    InvalidId,
    #[error("invalid memory record: {0}")]
    Invalid(String),
    #[error("missing subject key")]
    MissingSubjectKey,
    #[error("invalid title")]
    InvalidTitle,
    #[error("invalid subject")]
    InvalidSubject,
    #[error("invalid value")]
    InvalidValue,
    #[error("invalid tags")]
    InvalidTags,
    #[error("invalid revision")]
    InvalidRevision,
    #[error("experience memory is missing its experience block")]
    MissingExperience,
    #[error("user-fact memory cannot carry an experience block")]
    UnexpectedExperience,
    #[error("pending memory must describe a proposal")]
    MissingProposal,
    #[error("superseded memory must name its successor")]
    MissingSupersededBy,
    #[error("only superseded memory may name a successor")]
    UnexpectedSupersededBy,
    #[error("expired memory cannot remain active")]
    ExpiredButActive,
    #[error("pinned memory must name the pin owner")]
    MissingPinOwner,
    #[error("unpinned memory cannot name a pin owner")]
    UnexpectedPinOwner,
    #[error("evidence span is longer than allowed")]
    EvidenceTooLong,
    #[error("body is larger than allowed")]
    BodyTooLarge,
    #[error("credential-like value in field: {0}")]
    SecretDetected(&'static str),
}

pub fn sha256_hex(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    format!("{:x}", hasher.finalize())
}

pub fn sha256_hex_text(text: &str) -> String {
    sha256_hex(text.as_bytes())
}

/// Parse a complete Markdown memory document.
pub fn parse(bytes: &[u8]) -> Result<MemoryRecord, DocumentError> {
    if bytes.is_empty() {
        return Err(DocumentError::Empty);
    }
    if bytes.len() > MAX_FILE_BYTES {
        return Err(DocumentError::DocumentTooLarge);
    }
    if bytes.starts_with(&[0xEF, 0xBB, 0xBF]) {
        return Err(DocumentError::ByteOrderMark);
    }
    let text = std::str::from_utf8(bytes).map_err(|_| DocumentError::NotUtf8)?;
    parse_text(text)
}

pub fn parse_text(text: &str) -> Result<MemoryRecord, DocumentError> {
    if text.len() > MAX_FILE_BYTES {
        return Err(DocumentError::DocumentTooLarge);
    }
    let rest = text
        .strip_prefix("---\n")
        .ok_or(DocumentError::MissingFrontmatter)?;
    let close = find_closing_delimiter(rest).ok_or(DocumentError::UnterminatedFrontmatter)?;
    let (frontmatter_text, body) = rest.split_at(close);
    let body = body
        .strip_prefix("---\n")
        .expect("closing delimiter calculation");
    if frontmatter_text.len() > MAX_FRONTMATTER_BYTES {
        return Err(DocumentError::FrontmatterTooLarge);
    }
    if frontmatter_text.contains('\r') {
        return Err(DocumentError::UnsupportedTopLevelForm);
    }
    if frontmatter_text.contains('\0') || body.contains('\0') {
        return Err(DocumentError::Invalid("NUL byte".into()));
    }
    validate_top_level_keys(frontmatter_text)?;
    if frontmatter_text.contains("---") {
        // A nested document marker would make the closing delimiter ambiguous.
        return Err(DocumentError::UnsupportedTopLevelForm);
    }
    let frontmatter: super::record::Frontmatter = serde_yaml::from_str(frontmatter_text)
        .map_err(|error| DocumentError::Yaml(error.to_string()))?;
    if frontmatter.schema != SCHEMA_VERSION {
        return Err(DocumentError::UnsupportedSchema(frontmatter.schema));
    }
    if frontmatter.id.as_str().len() > MAX_ID_BYTES {
        return Err(DocumentError::InvalidId);
    }
    let record = MemoryRecord {
        frontmatter,
        body: body.to_string(),
    };
    record.validate()?;
    Ok(record)
}

pub fn render(record: &MemoryRecord) -> Result<Vec<u8>, DocumentError> {
    record.validate()?;
    let yaml = serde_yaml::to_string(&record.frontmatter)
        .map_err(|error| DocumentError::Yaml(error.to_string()))?;
    if yaml.len() > MAX_FRONTMATTER_BYTES {
        return Err(DocumentError::FrontmatterTooLarge);
    }
    let mut output = String::with_capacity(yaml.len() + record.body.len() + 8);
    output.push_str("---\n");
    output.push_str(&yaml);
    output.push_str("---\n");
    output.push_str(&record.body);
    let bytes = output.into_bytes();
    if bytes.len() > MAX_FILE_BYTES {
        return Err(DocumentError::DocumentTooLarge);
    }
    Ok(bytes)
}

pub fn render_text(record: &MemoryRecord) -> Result<String, DocumentError> {
    let bytes = render(record)?;
    String::from_utf8(bytes).map_err(|_| DocumentError::NotUtf8)
}

/// Find the byte offset of the closing `---\n` line. The returned offset points
/// at the start of the delimiter line.
fn find_closing_delimiter(rest: &str) -> Option<usize> {
    let mut offset = 0usize;
    for line in rest.split_inclusive('\n') {
        let content = line.strip_suffix('\n').unwrap_or(line);
        if content == "---" {
            return Some(offset);
        }
        offset += line.len();
    }
    None
}

fn validate_top_level_keys(frontmatter: &str) -> Result<(), DocumentError> {
    let mut keys = HashSet::new();
    for raw_line in frontmatter.split('\n') {
        let line = raw_line.trim_end_matches('\r');
        if line.trim().is_empty() || line.trim_start().starts_with('#') {
            continue;
        }
        if line.starts_with(' ') || line.starts_with('\t') || line.starts_with('-') {
            continue;
        }
        let key = top_level_key(line).ok_or(DocumentError::UnsupportedTopLevelForm)?;
        if key == "<<" {
            return Err(DocumentError::AliasOrAnchor);
        }
        if !keys.insert(key.to_string()) {
            return Err(DocumentError::DuplicateKey(key.to_string()));
        }
        let value = line
            .split_once(':')
            .map(|(_, value)| value.trim_start())
            .unwrap_or("");
        if value.starts_with('&') || value.starts_with('*') || value.starts_with("!!") {
            return Err(DocumentError::AliasOrAnchor);
        }
    }
    // Nested anchors/aliases are rejected conservatively: this store only emits
    // plain scalars, sequences and mappings.
    let bytes = frontmatter.as_bytes();
    for (index, byte) in bytes.iter().enumerate() {
        if *byte != b'&' && *byte != b'*' {
            continue;
        }
        let previous = index.checked_sub(1).and_then(|i| bytes.get(i)).copied();
        let next = bytes.get(index + 1).copied();
        let boundary = previous.is_none_or(|b| {
            b.is_ascii_whitespace() || matches!(b, b':' | b',' | b'[' | b'{' | b'-')
        });
        let token = next.is_some_and(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-');
        if boundary && token {
            return Err(DocumentError::AliasOrAnchor);
        }
    }
    Ok(())
}

fn top_level_key(line: &str) -> Option<&str> {
    if let Some(rest) = line.strip_prefix('"') {
        let end = rest.find('"')?;
        let key = &rest[..end];
        let after = rest[end + 1..].strip_prefix(':')?;
        if after.is_empty() || after.starts_with(' ') {
            return Some(key);
        }
        return None;
    }
    if let Some(rest) = line.strip_prefix('\'') {
        let end = rest.find('\'')?;
        let key = &rest[..end];
        let after = rest[end + 1..].strip_prefix(':')?;
        if after.is_empty() || after.starts_with(' ') {
            return Some(key);
        }
        return None;
    }
    let (key, after) = line.split_once(':')?;
    if key.is_empty() || key.contains(' ') || key.contains('\t') {
        return None;
    }
    if !after.is_empty() && !after.starts_with(' ') {
        return None;
    }
    Some(key)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::v2::record::{
        Frontmatter, MemoryAuthority, MemoryId, MemoryKind, MemoryRecord, MemoryScope,
        MemoryStatus, StatementKind,
    };
    use chrono::TimeZone;

    pub(crate) fn sample_record() -> MemoryRecord {
        let now = chrono::Utc
            .with_ymd_and_hms(2026, 10, 23, 10, 0, 0)
            .unwrap();
        MemoryRecord {
            frontmatter: Frontmatter {
                schema: 2,
                id: MemoryId::parse("mem_00000000000000000000000000000001").unwrap(),
                kind: MemoryKind::UserFact,
                scope: MemoryScope::Project,
                status: MemoryStatus::Active,
                revision: 1,
                authority: MemoryAuthority::UserExplicit,
                title: "Billing model".into(),
                subject: Some("provider billing model".into()),
                subject_key: Some("billing_model".into()),
                value: Some("monthly".into()),
                statement: StatementKind::Assertion,
                tags: vec!["billing".into()],
                pinned: false,
                pinned_by: None,
                sensitive: false,
                expires_at: None,
                supersedes: vec![],
                superseded_by: None,
                proposal: None,
                experience: None,
                source_refs: vec![],
                created_at: now,
                updated_at: now,
                extra: Default::default(),
            },
            body: "The provider bills monthly.\n".into(),
        }
    }

    #[test]
    fn round_trip_preserves_fields() {
        let record = sample_record();
        let text = render_text(&record).unwrap();
        let parsed = parse_text(&text).unwrap();
        assert_eq!(parsed.frontmatter.id, record.frontmatter.id);
        assert_eq!(
            parsed.frontmatter.subject_key.as_deref(),
            Some("billing_model")
        );
        assert_eq!(parsed.body, record.body);
    }

    #[test]
    fn unknown_fields_round_trip() {
        let mut record = sample_record();
        record.frontmatter.extra.insert(
            serde_yaml::Value::String("custom_note".into()),
            serde_yaml::Value::String("kept".into()),
        );
        let parsed = parse_text(&render_text(&record).unwrap()).unwrap();
        assert_eq!(
            parsed
                .frontmatter
                .extra
                .get(serde_yaml::Value::String("custom_note".into()))
                .and_then(serde_yaml::Value::as_str),
            Some("kept")
        );
    }

    #[test]
    fn duplicate_top_level_keys_are_rejected() {
        let text = "---\nschema: 2\nschema: 2\n---\n";
        assert!(matches!(
            parse_text(text),
            Err(DocumentError::DuplicateKey(_))
        ));
    }

    #[test]
    fn anchors_and_aliases_are_rejected() {
        let text = "---\ntitle: &a value\nschema: 2\n---\n";
        assert!(matches!(
            parse_text(text),
            Err(DocumentError::AliasOrAnchor)
        ));
    }

    #[test]
    fn bom_and_crlf_frontmatter_are_rejected() {
        assert!(matches!(
            parse(&[0xEF, 0xBB, 0xBF]),
            Err(DocumentError::ByteOrderMark)
        ));
        assert!(parse_text("---\r\nschema: 2\r\n---\r\n").is_err());
    }

    #[test]
    fn missing_frontmatter_is_rejected() {
        assert!(matches!(
            parse_text("# plain markdown\n"),
            Err(DocumentError::MissingFrontmatter)
        ));
    }
}
