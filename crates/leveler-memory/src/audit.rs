//! Read-only identity audit for historical memory.
//!
//! History predates the identity contract. This report says which records can
//! establish identity safely, which need a person, which appear to duplicate a
//! value under another key, which are stale copies, and which are withheld as
//! credentials. It NEVER writes: migration is a separate, explicit decision.

use std::collections::{BTreeMap, BTreeSet, HashMap};

use serde::Serialize;

use crate::{MemoryEntry, MemoryError, MemoryStore, is_sensitive};

/// What the audit can say about one historical record.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum IdentityState {
    /// A stable key already exists (or is derivable) and no conflict was seen.
    Ready,
    /// The record has no trustworthy identity. A person must decide.
    NeedsConfirmation,
    /// Another record states the same distinctive value under a different key.
    PossibleDuplicate,
    /// Another current record holds the same identity with a different value.
    PossiblyStale,
    /// Credential-shaped content; withheld from every automatic path.
    Sensitive,
}

impl IdentityState {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Ready => "ready",
            Self::NeedsConfirmation => "needs_confirmation",
            Self::PossibleDuplicate => "possible_duplicate",
            Self::PossiblyStale => "possibly_stale",
            Self::Sensitive => "sensitive",
        }
    }
}

/// Where the record lives. Archived records are history, not current truth.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum IdentityLocation {
    Active,
    Archived,
}

/// One audit line. It never carries a body or a value, so a credential cannot
/// leak through the report.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct IdentityRecord {
    pub id: String,
    pub location: IdentityLocation,
    /// The stored key, or one derived from an explicit durable assignment.
    pub key: Option<String>,
    /// Redacted when the record is sensitive.
    pub title: String,
    pub state: IdentityState,
    /// Ids this finding refers to.
    pub related: Vec<String>,
    pub reason: String,
    pub sensitive: bool,
}

/// Counts by final state. Every record contributes exactly once.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
pub struct IdentitySummary {
    pub total: usize,
    pub ready: usize,
    pub needs_confirmation: usize,
    pub possible_duplicate: usize,
    pub possibly_stale: usize,
    pub sensitive: usize,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct IdentityAudit {
    pub records: Vec<IdentityRecord>,
    pub summary: IdentitySummary,
}

impl MemoryStore {
    /// Audit active and archived records without modifying anything.
    pub fn audit_identity(&self) -> Result<IdentityAudit, MemoryError> {
        let mut records = Vec::new();
        let active = self.list_active()?;
        let archived = self.list_archived()?;

        let active_current: HashMap<String, String> = self
            .effective_active()?
            .into_iter()
            .filter_map(|entry| {
                crate::lifecycle::durable_entry_key(&entry).map(|key| (key, entry.id))
            })
            .collect();

        for entry in &active {
            records.push(classify(entry, IdentityLocation::Active, &active_current));
        }
        for entry in &archived {
            records.push(classify(entry, IdentityLocation::Archived, &active_current));
        }

        mark_shared_values(&mut records, &active);
        mark_stale_active_copies(&mut records, &active_current);
        records.sort_by(|left, right| {
            location_rank(left.location)
                .cmp(&location_rank(right.location))
                .then_with(|| left.title.cmp(&right.title))
                .then_with(|| left.id.cmp(&right.id))
        });

        let mut summary = IdentitySummary {
            total: records.len(),
            ..IdentitySummary::default()
        };
        for record in &records {
            match record.state {
                IdentityState::Ready => summary.ready += 1,
                IdentityState::NeedsConfirmation => summary.needs_confirmation += 1,
                IdentityState::PossibleDuplicate => summary.possible_duplicate += 1,
                IdentityState::PossiblyStale => summary.possibly_stale += 1,
                IdentityState::Sensitive => summary.sensitive += 1,
            }
        }
        Ok(IdentityAudit { records, summary })
    }
}

fn location_rank(location: IdentityLocation) -> u8 {
    match location {
        IdentityLocation::Active => 0,
        IdentityLocation::Archived => 1,
    }
}

fn classify(
    entry: &MemoryEntry,
    location: IdentityLocation,
    current_by_key: &HashMap<String, String>,
) -> IdentityRecord {
    let sensitive = is_sensitive(entry);
    let key = crate::lifecycle::durable_entry_key(entry);
    let key_is_canonical = key.as_deref().is_some_and(is_canonical_key);

    let (state, reason) = if sensitive {
        (
            IdentityState::Sensitive,
            "credential-shaped content is withheld".to_string(),
        )
    } else if key.is_none() {
        (
            IdentityState::NeedsConfirmation,
            "no trustworthy semantic identity; a person must decide".to_string(),
        )
    } else if !key_is_canonical {
        (
            IdentityState::NeedsConfirmation,
            "the stored key is not a normalized identity".to_string(),
        )
    } else if location == IdentityLocation::Archived {
        (
            IdentityState::Ready,
            "historical record with a stable identity".to_string(),
        )
    } else if let Some(current) = key
        .as_deref()
        .and_then(|key| current_by_key.get(key))
        .filter(|current| **current != entry.id)
    {
        (
            IdentityState::PossiblyStale,
            format!("another current record holds this identity: {current}"),
        )
    } else {
        (
            IdentityState::Ready,
            "current value with a stable identity".to_string(),
        )
    };

    IdentityRecord {
        id: entry.id.clone(),
        location,
        key: if sensitive {
            key.filter(|key| !crate::looks_like_secret(key))
        } else {
            key
        },
        title: if sensitive {
            crate::REDACTED_TITLE.to_string()
        } else {
            entry.title.clone()
        },
        state,
        related: Vec::new(),
        reason,
        sensitive,
    }
}

fn is_canonical_key(key: &str) -> bool {
    key.starts_with("k-")
        && key.len() > 2
        && key
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

/// Flag active records that share a distinctive value under DIFFERENT keys.
/// This is deliberately a report, not a merge: similar wording is not identity.
fn mark_shared_values(records: &mut [IdentityRecord], entries: &[MemoryEntry]) {
    let by_id: HashMap<&str, &MemoryEntry> = entries
        .iter()
        .map(|entry| (entry.id.as_str(), entry))
        .collect();
    let mut by_token: BTreeMap<String, BTreeSet<usize>> = BTreeMap::new();
    for (index, record) in records.iter().enumerate() {
        if record.sensitive || record.location != IdentityLocation::Active {
            continue;
        }
        let Some(entry) = by_id.get(record.id.as_str()) else {
            continue;
        };
        for token in distinctive_tokens(&format!("{} {}", entry.title, entry.body)) {
            by_token.entry(token).or_default().insert(index);
        }
    }

    let mut related: BTreeMap<usize, BTreeSet<String>> = BTreeMap::new();
    for indexes in by_token.values() {
        if indexes.len() < 2 {
            continue;
        }
        // Only a cross-key match is evidence of duplication; the same key is
        // already an identity conflict, not a wording coincidence.
        for &left in indexes {
            for &right in indexes {
                if left == right {
                    continue;
                }
                if records[left].key == records[right].key {
                    continue;
                }
                related
                    .entry(left)
                    .or_default()
                    .insert(records[right].id.clone());
            }
        }
    }

    for (index, ids) in related {
        let record = &mut records[index];
        if record.state == IdentityState::Sensitive {
            continue;
        }
        record.state = IdentityState::PossibleDuplicate;
        record.related = ids.into_iter().collect();
        record.reason = "shares a distinctive value with another identity".to_string();
    }
}

/// Active residue under one identity: only the current entry is truth.
fn mark_stale_active_copies(
    records: &mut [IdentityRecord],
    current_by_key: &HashMap<String, String>,
) {
    for record in records {
        if record.location != IdentityLocation::Active || record.sensitive {
            continue;
        }
        let Some(current) = record
            .key
            .as_deref()
            .and_then(|key| current_by_key.get(key))
            .filter(|current| **current != record.id)
        else {
            continue;
        };
        record.state = IdentityState::PossiblyStale;
        record.related = vec![current.clone()];
        record.reason = format!("a stale active copy; current truth is {current}");
    }
}

/// Tokens that look like a value rather than ordinary prose. Values are what a
/// duplicate detector can compare without resorting to fuzzy text similarity.
fn distinctive_tokens(text: &str) -> BTreeSet<String> {
    const STOPWORDS: &[&str] = &[
        "about",
        "after",
        "allowed",
        "and",
        "api",
        "base",
        "before",
        "branch",
        "coding",
        "compact",
        "config",
        "configuration",
        "default",
        "deploy",
        "enabled",
        "false",
        "file",
        "files",
        "for",
        "from",
        "leveler",
        "memory",
        "model",
        "models",
        "output",
        "path",
        "plan",
        "production",
        "project",
        "provider",
        "release",
        "service",
        "setting",
        "should",
        "staging",
        "terminal",
        "the",
        "token",
        "true",
        "url",
        "using",
        "value",
        "with",
        "workspace",
    ];
    let mut out = BTreeSet::new();
    for raw in text.split(|c: char| !c.is_ascii_alphanumeric()) {
        let token = raw.to_ascii_lowercase();
        if token.len() < 4 || STOPWORDS.contains(&token.as_str()) {
            continue;
        }
        let has_digit = token.chars().any(|c| c.is_ascii_digit());
        if has_digit || token.len() >= 6 {
            out.insert(token);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{MemoryAuthority, Provenance, new_entry};
    use tempfile::tempdir;

    fn store_with(entries: Vec<MemoryEntry>) -> (tempfile::TempDir, MemoryStore) {
        let dir = tempdir().unwrap();
        let store = MemoryStore::open(dir.path()).unwrap();
        // Historical data may contain records the current write boundary would
        // refuse, so the fixture places files directly.
        for entry in entries {
            std::fs::write(
                dir.path().join("active").join(format!("{}.json", entry.id)),
                serde_json::to_string_pretty(&entry).unwrap(),
            )
            .unwrap();
        }
        (dir, store)
    }

    fn legacy(id: &str, key: Option<&str>, title: &str, body: &str) -> MemoryEntry {
        let mut entry = new_entry(title, body, vec![]);
        entry.id = id.to_string();
        entry.key = key.map(str::to_string);
        entry.kind = Some("note".into());
        entry.provenance = Some(Provenance {
            authority: MemoryAuthority::ExplicitUser,
            source: "explicit_user_turn".into(),
            evidence: None,
        });
        entry
    }

    /// The audit is a report: reading it must not create, rewrite or archive a
    /// single file, and it must never print a credential or a body.
    #[test]
    fn the_identity_audit_is_read_only_and_withholds_sensitive_content() {
        let secret = "ghp_abcdefghijklmnopqrstuvwxyz0123";
        let (dir, store) = store_with(vec![
            legacy(
                "k-apibaseurl",
                Some("k-apibaseurl"),
                "coding plan API base URL",
                "coding plan API base URL is https://ark.cn-beijing.volces.com/api/coding/v3",
            ),
            legacy(
                "k-llmprovidervolcanoendpoint",
                Some("k-llmprovidervolcanoendpoint"),
                "编码计划服务端点",
                "编码计划服务端点使用 https://ark.cn-beijing.volces.com/api/coding/v3",
            ),
            legacy(
                "k-llmprovidervolcanoapikey",
                Some("k-llmprovidervolcanoapikey"),
                "火山 coding plan 的 API Key",
                &format!("API Key 为 {secret}"),
            ),
            legacy(
                "k-unknown",
                None,
                "Some historical note",
                "A note without an explicit assignment.",
            ),
        ]);

        let before: Vec<Vec<u8>> = ["active", "archive", "pending"]
            .iter()
            .flat_map(|folder| {
                std::fs::read_dir(dir.path().join(folder))
                    .unwrap()
                    .filter_map(|entry| entry.ok())
                    .map(|entry| std::fs::read(entry.path()).unwrap())
                    .collect::<Vec<_>>()
            })
            .collect();

        let audit = store.audit_identity().unwrap();
        let rendered = serde_json::to_string(&audit).unwrap();
        assert!(!rendered.contains(secret), "audit leaked a credential");
        assert!(!rendered.contains("A note without an explicit assignment."));
        assert_eq!(audit.summary.total, 4);
        assert_eq!(audit.summary.sensitive, 1);
        assert_eq!(audit.summary.possible_duplicate, 2, "{:?}", audit.records);
        assert_eq!(audit.summary.needs_confirmation, 1);
        let sensitive = audit
            .records
            .iter()
            .find(|record| record.id == "k-llmprovidervolcanoapikey")
            .unwrap();
        assert_eq!(sensitive.title, crate::REDACTED_TITLE);

        let after: Vec<Vec<u8>> = ["active", "archive", "pending"]
            .iter()
            .flat_map(|folder| {
                std::fs::read_dir(dir.path().join(folder))
                    .unwrap()
                    .filter_map(|entry| entry.ok())
                    .map(|entry| std::fs::read(entry.path()).unwrap())
                    .collect::<Vec<_>>()
            })
            .collect();
        assert_eq!(before, after, "a dry run must not touch user data");
    }

    /// Two active copies of one identity: the audit names the terminal value
    /// as ready and the residue as stale.
    #[test]
    fn stale_residue_is_reported_without_being_repaired() {
        let mut old = legacy("old-pro", Some("k-defaultmodel"), "A old", "默认模型：Pro");
        old.updated_at = "2026-01-01T00:00:00Z".into();
        old.created_at = old.updated_at.clone();
        let mut new = legacy(
            "new-flash",
            Some("k-defaultmodel"),
            "B new",
            "默认模型：Flash",
        );
        new.updated_at = "2026-01-01T00:00:00Z".into();
        new.created_at = new.updated_at.clone();
        new.supersedes = Some("old-pro".into());
        let (_dir, store) = store_with(vec![old, new]);

        let audit = store.audit_identity().unwrap();
        let old = audit.records.iter().find(|r| r.id == "old-pro").unwrap();
        let new = audit.records.iter().find(|r| r.id == "new-flash").unwrap();
        assert_eq!(old.state, IdentityState::PossiblyStale, "{:?}", old);
        assert_eq!(old.related, vec!["new-flash".to_string()]);
        assert_eq!(new.state, IdentityState::Ready, "{:?}", new);
        // The residue is still on disk: the report did not repair it.
        assert_eq!(store.list_active().unwrap().len(), 2);
    }

    /// The shape of the user's nine historical records: two credential-bearing
    /// copies, overlapping provider facts under different keys, and two
    /// independent facts. The audit must describe all of it without touching a
    /// file or printing a value.
    #[test]
    fn nine_historical_records_are_classified_read_only() {
        let secret = "opaque-provider-value-1234567890";
        let url = "https://ark.cn-beijing.volces.com/api/coding/v3";
        let (dir, store) = store_with(vec![
            legacy(
                "k-apibaseurl",
                Some("k-apibaseurl"),
                "coding plan API base URL",
                &format!("coding plan API base URL is {url}"),
            ),
            legacy(
                "k-apikey",
                Some("k-apikey"),
                "coding plan API key",
                &format!("coding plan API key is {secret}"),
            ),
            legacy(
                "k-apimodels",
                Some("k-apimodels"),
                "models GLM5.3 and deepseek are allowed",
                "models GLM5.3 and deepseek are allowed",
            ),
            legacy(
                "k-configfilepath",
                Some("k-configfilepath"),
                "配置写入配置文件",
                "配置写入配置文件 ~/.leveler/config.toml",
            ),
            legacy(
                "k-llmmodeldeepseekenabled",
                Some("k-llmmodeldeepseekenabled"),
                "可使用模型 deepseek",
                "可使用模型 deepseek",
            ),
            legacy(
                "k-llmmodelglm53enabled",
                Some("k-llmmodelglm53enabled"),
                "可使用模型 GLM5.3",
                "可使用模型 GLM5.3",
            ),
            legacy(
                "k-llmprovidervolcanoapikey",
                Some("k-llmprovidervolcanoapikey"),
                "火山 coding plan 的 API Key",
                &format!("火山 coding plan 的 API Key 为 {secret}"),
            ),
            legacy(
                "k-llmprovidervolcanoendpoint",
                Some("k-llmprovidervolcanoendpoint"),
                "编码计划服务端点",
                &format!("编码计划服务端点使用 {url}"),
            ),
            legacy(
                "k-llmprovidervolcanoplan",
                Some("k-llmprovidervolcanoplan"),
                "使用的是火山的 coding plan 服务",
                "使用的是火山的 coding plan 服务",
            ),
        ]);

        let before = std::fs::read_dir(dir.path().join("active"))
            .unwrap()
            .filter_map(|entry| entry.ok())
            .map(|entry| std::fs::read(entry.path()).unwrap())
            .collect::<Vec<_>>();
        let audit = store.audit_identity().unwrap();
        let rendered = serde_json::to_string(&audit).unwrap();
        let after = std::fs::read_dir(dir.path().join("active"))
            .unwrap()
            .filter_map(|entry| entry.ok())
            .map(|entry| std::fs::read(entry.path()).unwrap())
            .collect::<Vec<_>>();
        assert_eq!(before, after, "audit must be read-only");
        assert_eq!(audit.summary.total, 9);
        assert_eq!(audit.summary.sensitive, 2, "{rendered}");
        assert_eq!(audit.summary.ready, 2, "{rendered}");
        assert_eq!(audit.summary.possible_duplicate, 5, "{rendered}");
        assert_eq!(audit.summary.needs_confirmation, 0, "{rendered}");
        assert!(!rendered.contains(secret), "audit leaked a credential");
        assert!(!rendered.contains(url), "audit leaked a body value");
    }

    /// A credential-shaped key is sensitive content too; the report must not
    /// echo it even though the key normally appears in the audit.
    #[test]
    fn a_secret_key_is_withheld_from_the_audit() {
        let secret = "ghp_abcdefghijklmnopqrstuvwxyz0123";
        let mut entry = legacy("legacy-key", Some(secret), "Legacy note", "safe body");
        entry.key = Some(secret.to_string());
        let (_dir, store) = store_with(vec![entry]);
        let audit = store.audit_identity().unwrap();
        let rendered = serde_json::to_string(&audit).unwrap();
        assert!(!rendered.contains(secret), "{rendered}");
        assert_eq!(audit.records[0].state, IdentityState::Sensitive);
        assert!(audit.records[0].key.is_none());
    }

    #[test]
    fn an_explicit_assignment_without_a_stored_key_yields_a_derived_identity() {
        let (_dir, store) = store_with(vec![legacy(
            "historical",
            None,
            "旧默认模型",
            "以后本项目默认模型固定为 Pro",
        )]);
        let audit = store.audit_identity().unwrap();
        let record = &audit.records[0];
        assert_eq!(record.state, IdentityState::Ready, "{record:?}");
        assert_eq!(
            record.key.as_deref(),
            Some(crate::semantic_key_of("默认模型").as_str())
        );
    }
}
