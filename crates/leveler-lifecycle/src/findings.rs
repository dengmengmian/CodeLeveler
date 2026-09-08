//! Durable findings: what a child or the parent established, as information.
//!
//! One typed record per finding, living in [`crate::EvidenceLedger`] so it
//! persists and replays through the existing `EvidenceLedgerUpdated` events.
//!
//! A finding is a MODEL's conclusion in natural language. The runtime records
//! it, attributes it, and hands it to the parent — it does not judge it, track
//! its resolution, or let it gate completion. There used to be a six-state
//! machine here (Created → Acknowledged → Accepted → Addressed → Verified,
//! plus Rejected) with a `blocking` flag that refused `update_goal(complete)`,
//! and a host promotion that read a green `cargo test` as proof of a sentence
//! of English. None of that was mechanical truth.

use serde::{Deserialize, Serialize};

/// What kind of thing was found. Closed set — no taxonomy sprawl.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FindingKind {
    RelevantFile,
    RelevantSymbol,
    Dependency,
    Callsite,
    Risk,
    Test,
    Config,
    Observation,
    Correctness,
}

impl FindingKind {
    /// Parse a model-authored kind string. Unknown kinds are refused at the
    /// tool boundary, never silently coerced.
    pub fn parse(s: &str) -> Option<Self> {
        Some(match s.trim() {
            "relevant_file" => Self::RelevantFile,
            "relevant_symbol" => Self::RelevantSymbol,
            "dependency" => Self::Dependency,
            "callsite" => Self::Callsite,
            "risk" => Self::Risk,
            "test" => Self::Test,
            "config" => Self::Config,
            "observation" => Self::Observation,
            "correctness" => Self::Correctness,
            _ => return None,
        })
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::RelevantFile => "relevant_file",
            Self::RelevantSymbol => "relevant_symbol",
            Self::Dependency => "dependency",
            Self::Callsite => "callsite",
            Self::Risk => "risk",
            Self::Test => "test",
            Self::Config => "config",
            Self::Observation => "observation",
            Self::Correctness => "correctness",
        }
    }
}

/// One durable finding. Identity is `id`, assigned by the ledger that owns the
/// record (child ids never leak into the parent ledger — adoption re-keys).
///
/// Legacy snapshots carry `blocking` / `state` / `resolution_reason` fields;
/// serde ignores them on read, so an old session still replays.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FindingRecord {
    pub id: String,
    /// Child id (`agent-1`, `reviewer-…`) in the parent ledger; empty for a
    /// record still in the ledger of the agent that created it.
    #[serde(default)]
    pub source_child: String,
    /// Role of the reporter (`explorer` / `worker` / `reviewer` / `default`).
    #[serde(default)]
    pub role: String,
    pub kind: FindingKind,
    pub summary: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub file: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub symbol: Option<String>,
}

/// A compact, durable view of what one child produced — enough to trace its
/// contribution without copying its findings into an event.
///
/// Counts, not judgements: how many findings this child reported, and which
/// capability contract produced it. What the parent then did with them is the
/// parent's business and is visible in the transcript, not in a state field.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct ChildResultProjection {
    /// The child this projects. Joins to `FindingRecord::source_child`.
    pub child_id: String,
    pub role: String,
    /// Built-in capability contract that produced this child. Absent on
    /// events written before Child Profile existed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub profile_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub profile_role: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub capabilities: Vec<String>,
    /// Findings in the parent ledger attributed to this child.
    pub findings_total: u32,
}

impl ChildResultProjection {
    /// Project one child's contribution out of the parent's ledger findings.
    ///
    /// Pure: takes the records, returns counts. The caller owns where the
    /// records come from, so this is testable without a running agent.
    pub fn from_findings(child_id: &str, role: &str, findings: &[FindingRecord]) -> Self {
        Self {
            child_id: child_id.to_string(),
            role: role.to_string(),
            profile_id: None,
            profile_role: None,
            capabilities: Vec::new(),
            findings_total: findings
                .iter()
                .filter(|f| f.source_child == child_id)
                .count() as u32,
        }
    }

    /// Attach the child's capability contract. Counts stay as `from_findings`
    /// computed them; this is the join from role → profile for trace/eval.
    pub fn with_profile(
        mut self,
        profile_id: impl Into<String>,
        profile_role: impl Into<String>,
        capabilities: Vec<String>,
    ) -> Self {
        self.profile_id = Some(profile_id.into());
        self.profile_role = Some(profile_role.into());
        self.capabilities = capabilities;
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kinds_round_trip_between_parse_and_label() {
        for kind in [
            FindingKind::RelevantFile,
            FindingKind::RelevantSymbol,
            FindingKind::Dependency,
            FindingKind::Callsite,
            FindingKind::Risk,
            FindingKind::Test,
            FindingKind::Config,
            FindingKind::Observation,
            FindingKind::Correctness,
        ] {
            assert_eq!(FindingKind::parse(kind.label()), Some(kind));
        }
        assert_eq!(FindingKind::parse("nonsense"), None);
    }

    fn rec(id: &str, child: &str) -> FindingRecord {
        FindingRecord {
            id: id.to_string(),
            source_child: child.to_string(),
            role: "explorer".to_string(),
            kind: FindingKind::Risk,
            summary: format!("summary {id}"),
            file: None,
            symbol: None,
        }
    }

    #[test]
    fn a_projection_counts_only_its_own_child() {
        let findings = vec![rec("f-1", "a1"), rec("f-2", "a2"), rec("f-3", "a1")];
        let p = ChildResultProjection::from_findings("a1", "explorer", &findings);
        assert_eq!(p.findings_total, 2);
        assert_eq!(p.child_id, "a1");
    }

    #[test]
    fn a_child_with_no_findings_projects_a_measured_zero() {
        let p = ChildResultProjection::from_findings("a1", "explorer", &[]);
        assert_eq!(p.findings_total, 0);
        assert_eq!(p.profile_id, None, "not measured is the ABSENT projection");
    }

    #[test]
    fn records_round_trip_through_serde() {
        let r = rec("f-1", "a1");
        let json = serde_json::to_string(&r).unwrap();
        assert_eq!(serde_json::from_str::<FindingRecord>(&json).unwrap(), r);
    }

    /// A snapshot written before the lifecycle was deleted still replays: the
    /// dropped fields are ignored, not a decode failure.
    #[test]
    fn a_legacy_record_with_state_and_blocking_still_decodes() {
        let json = r#"{"id":"f-1","source_child":"a1","role":"reviewer",
            "kind":"correctness","summary":"boundary check missing",
            "blocking":true,"state":"accepted","resolution_reason":null}"#;
        let r: FindingRecord = serde_json::from_str(json).unwrap();
        assert_eq!(r.id, "f-1");
        assert_eq!(r.kind, FindingKind::Correctness);
    }

    #[test]
    fn a_projection_roundtrips_through_json() {
        let p = ChildResultProjection::from_findings("a1", "reviewer", &[rec("f-1", "a1")])
            .with_profile("reviewer", "reviewer", vec!["code_review".into()]);
        let json = serde_json::to_string(&p).unwrap();
        assert_eq!(
            serde_json::from_str::<ChildResultProjection>(&json).unwrap(),
            p
        );
    }

    /// An event written before profiles existed has no profile fields; that
    /// must read as "not recorded", never as a validation failure.
    #[test]
    fn a_legacy_projection_without_profile_fields_still_deserializes() {
        let p: ChildResultProjection = serde_json::from_str(
            r#"{"child_id":"a1","role":"explorer","findings_total":2}"#,
        )
        .unwrap();
        assert_eq!(p.findings_total, 2);
        assert_eq!(p.profile_id, None);
        assert!(p.capabilities.is_empty());
    }
}
