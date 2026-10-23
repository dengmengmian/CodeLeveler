//! Stage 1 schema and encoder contracts.

mod memory_v2_common;

use chrono::TimeZone;
use leveler_memory::v2::{
    DocumentError, Frontmatter, MemoryAuthority, MemoryId, MemoryKind, MemoryRecord, MemoryScope,
    MemoryStatus, StatementKind, frontmatter,
};

fn sample() -> MemoryRecord {
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
            subject: Some("billing model".into()),
            subject_key: Some("billing_model".into()),
            value: Some("monthly".into()),
            statement: StatementKind::Assertion,
            tags: vec!["billing".into()],
            pinned: false,
            pinned_by: None,
            sensitive: false,
            expires_at: None,
            supersedes: Vec::new(),
            superseded_by: None,
            proposal: None,
            experience: None,
            source_refs: Vec::new(),
            created_at: now,
            updated_at: now,
            extra: Default::default(),
        },
        body: "The provider bills monthly.\n".into(),
    }
}

#[test]
fn schema_round_trip_preserves_unknown_fields() {
    let mut record = sample();
    record.frontmatter.extra.insert(
        serde_yaml::Value::String("custom_note".into()),
        serde_yaml::Value::String("kept".into()),
    );
    let text = frontmatter::render_text(&record).unwrap();
    let parsed = frontmatter::parse_text(&text).unwrap();
    assert_eq!(parsed.frontmatter.id, record.frontmatter.id);
    assert_eq!(
        parsed
            .frontmatter
            .extra
            .get(serde_yaml::Value::String("custom_note".into()))
            .and_then(serde_yaml::Value::as_str),
        Some("kept")
    );
    assert_eq!(parsed.body, record.body);
}

#[test]
fn schema_rejects_duplicate_top_level_keys() {
    let text = "---\nschema: 2\nschema: 2\n---\n";
    assert!(matches!(
        frontmatter::parse_text(text),
        Err(DocumentError::DuplicateKey(_))
    ));
}

#[test]
fn schema_rejects_anchors_and_aliases() {
    let text = "---\ntitle: &a value\nschema: 2\n---\n";
    assert!(matches!(
        frontmatter::parse_text(text),
        Err(DocumentError::AliasOrAnchor)
    ));
}

#[test]
fn schema_rejects_bom_and_crlf_frontmatter() {
    assert!(matches!(
        frontmatter::parse(&[0xEF, 0xBB, 0xBF]),
        Err(DocumentError::ByteOrderMark)
    ));
    assert!(frontmatter::parse_text("---\r\nschema: 2\r\n---\r\n").is_err());
}

#[test]
fn schema_rejects_missing_frontmatter_and_oversized_documents() {
    assert!(matches!(
        frontmatter::parse_text("# plain markdown\n"),
        Err(DocumentError::MissingFrontmatter)
    ));
    let oversized = vec![b'a'; 128 * 1024 + 1];
    assert!(matches!(
        frontmatter::parse(&oversized),
        Err(DocumentError::DocumentTooLarge)
    ));
}

#[test]
fn schema_rejects_unsupported_schema_versions() {
    let text = "---\nschema: 1\nid: mem_00000000000000000000000000000001\nkind: user_fact\nscope: project\nstatus: active\nrevision: 1\nauthority: user_explicit\ntitle: Old\nsubject_key: old\ncreated_at: 2026-10-23T10:00:00Z\nupdated_at: 2026-10-23T10:00:00Z\n---\nbody\n";
    assert!(matches!(
        frontmatter::parse_text(text),
        Err(DocumentError::UnsupportedSchema(1))
    ));
}

#[test]
fn schema_rejects_credentials_in_any_field() {
    let mut record = sample();
    record.body = "api_key=sk-live-123\n".into();
    assert!(matches!(
        frontmatter::render(&record),
        Err(DocumentError::SecretDetected("body"))
    ));

    record.body = "ordinary body".into();
    record.frontmatter.value = Some("client_secret=abc".into());
    assert!(matches!(
        frontmatter::render(&record),
        Err(DocumentError::SecretDetected("value"))
    ));

    record.frontmatter.value = Some("monthly".into());
    let mut extra = serde_yaml::Mapping::new();
    extra.insert(
        serde_yaml::Value::String("note".into()),
        serde_yaml::Value::String("token=ghp_0123456789".into()),
    );
    record.frontmatter.source_refs = vec![leveler_memory::v2::SourceRef {
        kind: leveler_memory::v2::SourceRefKind::UserTurn,
        session_id: Some("session-1".into()),
        turn_id: None,
        tool_call_id: None,
        task_id: None,
        pending_id: None,
        evidence_span: None,
        message_digest: None,
        span_range: None,
        file_sha256: None,
        legacy_store: None,
        legacy_id: None,
        legacy_sha256: None,
        extra,
    }];
    assert!(matches!(
        frontmatter::render(&record),
        Err(DocumentError::SecretDetected("source_refs"))
    ));
}

#[test]
fn ids_reject_path_characters() {
    assert!(MemoryId::parse("../escape").is_err());
    assert!(MemoryId::parse("mem/child").is_err());
    assert!(leveler_memory::v2::OpId::parse("../../op").is_err());
}

#[test]
fn local_scope_requires_a_repository_root() {
    let dir = tempfile::tempdir().unwrap();
    let roots = leveler_memory::v2::ScopeRoots::new(
        MemoryScope::Local,
        dir.path().join("current"),
        dir.path().join("state"),
    );
    assert!(matches!(
        leveler_memory::v2::StorePaths::prepare(&roots),
        Err(leveler_memory::v2::PathError::ScopeUnavailable(_))
    ));
}

#[cfg(unix)]
#[test]
fn symlinked_current_root_is_rejected() {
    use std::os::unix::fs::symlink;

    let dir = tempfile::tempdir().unwrap();
    let target = dir.path().join("target");
    std::fs::create_dir(&target).unwrap();
    let current = dir.path().join("current");
    symlink(&target, &current).unwrap();
    let roots = leveler_memory::v2::ScopeRoots::new(
        MemoryScope::Project,
        &current,
        dir.path().join("state"),
    );
    assert!(matches!(
        leveler_memory::v2::StorePaths::prepare(&roots),
        Err(leveler_memory::v2::PathError::Symlink(_))
    ));
}
