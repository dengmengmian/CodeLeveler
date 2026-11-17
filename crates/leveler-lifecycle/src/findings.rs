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

/// Which kind of review produced a finding. Independent of [`FindingKind`]:
/// `kind` says what was found, `dimension` says which review lens found it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReviewDimension {
    Code,
    Business,
    Security,
    Architecture,
    Test,
    Delivery,
    Session,
}

impl ReviewDimension {
    pub fn parse(s: &str) -> Option<Self> {
        Some(match s.trim().to_ascii_lowercase().as_str() {
            "code" => Self::Code,
            "business" => Self::Business,
            "security" => Self::Security,
            "architecture" => Self::Architecture,
            "test" | "tests" => Self::Test,
            "delivery" => Self::Delivery,
            "session" => Self::Session,
            _ => return None,
        })
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Code => "code",
            Self::Business => "business",
            Self::Security => "security",
            Self::Architecture => "architecture",
            Self::Test => "test",
            Self::Delivery => "delivery",
            Self::Session => "session",
        }
    }
}

/// How bad the finding would be if it is real. Separate from confidence.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FindingSeverity {
    P0,
    P1,
    P2,
    P3,
}

impl FindingSeverity {
    pub fn parse(s: &str) -> Option<Self> {
        Some(match s.trim().to_ascii_lowercase().as_str() {
            "p0" => Self::P0,
            "p1" => Self::P1,
            "p2" => Self::P2,
            "p3" => Self::P3,
            _ => return None,
        })
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::P0 => "p0",
            Self::P1 => "p1",
            Self::P2 => "p2",
            Self::P3 => "p3",
        }
    }
}

/// How sure the reviewer is that the finding is real.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FindingConfidence {
    High,
    Medium,
    Low,
}

impl FindingConfidence {
    pub fn parse(s: &str) -> Option<Self> {
        Some(match s.trim().to_ascii_lowercase().as_str() {
            "high" => Self::High,
            "medium" => Self::Medium,
            "low" => Self::Low,
            _ => return None,
        })
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::High => "high",
            Self::Medium => "medium",
            Self::Low => "low",
        }
    }
}

/// How far one review dimension was actually covered.
///
/// `NotApplied` means the dimension was outside this review's declared scope;
/// `NotReviewed` means it was in scope but not examined. Neither is a claim
/// about correctness.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReviewCoverageStatus {
    NotApplicable,
    NotReviewed,
    Partial,
    Reviewed,
}

impl ReviewCoverageStatus {
    /// Parse a model-authored status. Unknown values are refused, never
    /// coerced to a reassuring default.
    pub fn parse(s: &str) -> Option<Self> {
        Some(match s.trim().to_ascii_lowercase().as_str() {
            "not_applicable" => Self::NotApplicable,
            "not_reviewed" => Self::NotReviewed,
            "partial" => Self::Partial,
            "reviewed" => Self::Reviewed,
            _ => return None,
        })
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::NotApplicable => "not_applicable",
            Self::NotReviewed => "not_reviewed",
            Self::Partial => "partial",
            Self::Reviewed => "reviewed",
        }
    }
}

/// The reviewer's own final verdict, kept separate from execution status.
///
/// `IssuesFound` does not change the reviewed task's terminal; `NoIssuesFound`
/// does not prove the object correct. The runtime stores the declaration, it
/// never derives one from finding counts or report prose.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReviewConclusion {
    IssuesFound,
    NoIssuesFound,
    Inconclusive,
}

impl ReviewConclusion {
    pub fn parse(s: &str) -> Option<Self> {
        Some(match s.trim().to_ascii_lowercase().as_str() {
            "issues_found" => Self::IssuesFound,
            "no_issues_found" => Self::NoIssuesFound,
            "inconclusive" => Self::Inconclusive,
            _ => return None,
        })
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::IssuesFound => "issues_found",
            Self::NoIssuesFound => "no_issues_found",
            Self::Inconclusive => "inconclusive",
        }
    }
}

/// Why a review could not establish something. The kind separates what the
/// reviewer chose not to cover from what the environment or runtime made
/// impossible.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReviewLimitationKind {
    /// The surrounding environment (no database, no network, no production
    /// configuration) prevented the check.
    Environment,
    /// Authority or approval boundaries prevented the check.
    Permission,
    /// The review surface itself has no such capability (for example: it holds
    /// no command tool, so it cannot run tests).
    ToolCapability,
    /// The user's requested scope did not include this area.
    Scope,
    /// The reviewed object moved after capture.
    ObjectChanged,
    Other,
}

impl ReviewLimitationKind {
    pub fn parse(s: &str) -> Option<Self> {
        Some(match s.trim().to_ascii_lowercase().as_str() {
            "environment" => Self::Environment,
            "permission" => Self::Permission,
            "tool_capability" | "tool" => Self::ToolCapability,
            "scope" => Self::Scope,
            "object_changed" => Self::ObjectChanged,
            "other" => Self::Other,
            _ => return None,
        })
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Environment => "environment",
            Self::Permission => "permission",
            Self::ToolCapability => "tool_capability",
            Self::Scope => "scope",
            Self::ObjectChanged => "object_changed",
            Self::Other => "other",
        }
    }
}

/// One declared limitation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReviewLimitation {
    pub kind: ReviewLimitationKind,
    pub detail: String,
}

/// One dimension's declared coverage.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReviewCoverage {
    pub dimension: ReviewDimension,
    pub status: ReviewCoverageStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scope: Option<String>,
    /// Concrete references (file:line, event sequence, command output) the
    /// reviewer used. Not a proof of correctness.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub evidence_refs: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limitations: Option<String>,
}

/// The structured part of a review report: what the reviewer claims to have
/// covered, what stopped it, and its conclusion. Only the `report_review`
/// tool writes this; the final prose is never parsed for these fields.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReviewVerdict {
    pub conclusion: ReviewConclusion,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub coverage: Vec<ReviewCoverage>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub limitations: Vec<ReviewLimitation>,
    /// The complete formal report body, submitted by the Lead Reviewer through
    /// `report_review`. This is the report of record; the final chat message is
    /// never parsed or substituted for it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub report: Option<String>,
    /// Revision number, stamped by the runtime on every `report_review` call
    /// (1-based). The model cannot set it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub report_revision: Option<u32>,
    /// The evidence revision this submission was based on, stamped by the
    /// runtime. If the review later receives structured evidence, the report
    /// no longer covers the latest set.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub evidence_revision: Option<u64>,
}

/// One item of evidence an investigation cites. Model-authored: the runtime
/// records it, attributes it, and never treats it as proof by itself.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InvestigationFact {
    pub statement: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub evidence_refs: Vec<String>,
}

/// What an independent investigation established, as data rather than prose.
///
/// This is the child's own declaration, written only by the
/// `report_investigation` tool. The parent reviewer reads it in the settlement
/// notice and decides what to believe; it is evidence, not a verdict, and it
/// never replaces `ReviewVerdict`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct InvestigationRecord {
    /// The delegated objective, in the child's words.
    pub objective: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub observed_facts: Vec<InvestigationFact>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub business_constraints: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub examined_scope: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub unresolved_questions: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub limitations: Vec<String>,
    /// Global references the child used. Per-fact references live on the fact.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub evidence_refs: Vec<String>,
    /// The fixed reviewed object this investigation was bound to. Stamped by
    /// the runtime from the review context, never by the model.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub review_revision: Option<String>,
}

/// How the Lead Reviewer disposed of a finding that arrived from a child.
///
/// Absent means the record was reported by the seat that owns the ledger (a
/// Lead Reviewer's own finding, or an ordinary child's finding before its
/// parent adopted it). `Candidate` is an Investigator's finding the Lead has
/// not adjudicated; it is evidence, never a formal defect. Only `Adopted` (and
/// an absent disposition) count as formal review findings.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FindingDisposition {
    /// Proposed by an Investigator; the Lead has not decided yet.
    Candidate,
    /// Explicitly adopted by the Lead as a formal finding.
    Adopted,
    /// Explicitly rejected by the Lead; kept for traceability.
    Rejected,
    /// Merged into another finding; `duplicate_of` names the survivor.
    Duplicate,
}

impl FindingDisposition {
    pub fn parse(s: &str) -> Option<Self> {
        Some(match s.trim().to_ascii_lowercase().as_str() {
            "candidate" | "unresolved" => Self::Candidate,
            "adopted" => Self::Adopted,
            "rejected" => Self::Rejected,
            "duplicate" => Self::Duplicate,
            _ => return None,
        })
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Candidate => "candidate",
            Self::Adopted => "adopted",
            Self::Rejected => "rejected",
            Self::Duplicate => "duplicate",
        }
    }
}

/// Optional review fields on a finding. Every field is defaulted and optional
/// so an explorer/worker `report_finding` call keeps exactly its old shape.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct FindingDetails {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dimension: Option<ReviewDimension>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub severity: Option<FindingSeverity>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub confidence: Option<FindingConfidence>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub line: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub evidence: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub impact: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub recommendation: Option<String>,
    #[serde(default, skip_serializing_if = "is_false")]
    pub reproduced: bool,
    /// The Lead Reviewer's disposition of a finding. Absent when the record was
    /// reported by the seat that owns the ledger.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub disposition: Option<FindingDisposition>,
    /// Why the Lead adopted, rejected or merged the candidate.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub disposition_reason: Option<String>,
    /// The references the Lead used for that decision.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub disposition_evidence: Vec<String>,
    /// The surviving finding when this one was merged as a duplicate.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub duplicate_of: Option<String>,
    /// The Investigator's original finding id, preserved across adoption so
    /// the candidate can be traced back to its own ledger.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_finding_id: Option<String>,
}

fn is_false(value: &bool) -> bool {
    !*value
}

/// The Lead Reviewer's structured decision about one candidate finding.
///
/// Parsed from `adjudicate_finding`, applied by the ledger that owns the
/// record. `Candidate` means "still unresolved": it clears an earlier
/// decision without adopting or rejecting the finding.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FindingAdjudication {
    pub disposition: FindingDisposition,
    pub reason: Option<String>,
    pub evidence: Vec<String>,
    /// Required for [`FindingDisposition::Duplicate`]: the finding that
    /// represents this root cause.
    pub duplicate_of: Option<String>,
    pub severity: Option<FindingSeverity>,
    pub confidence: Option<FindingConfidence>,
    pub title: Option<String>,
}

impl Default for FindingAdjudication {
    fn default() -> Self {
        Self {
            disposition: FindingDisposition::Candidate,
            reason: None,
            evidence: Vec::new(),
            duplicate_of: None,
            severity: None,
            confidence: None,
            title: None,
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
    /// Review fields. Absent on every finding produced before manual review,
    /// and on ordinary explorer/worker reports.
    #[serde(flatten, default)]
    pub details: FindingDetails,
}

impl FindingRecord {
    /// Whether this record belongs to the review's FORMAL finding set.
    ///
    /// A Lead Reviewer's own finding and an explicitly adopted candidate are
    /// formal. A candidate is pending, a rejected one is not a defect, and a
    /// duplicate is represented by its surviving finding.
    pub fn is_formal(&self) -> bool {
        matches!(
            self.details.disposition,
            None | Some(FindingDisposition::Adopted)
        )
    }

    /// Whether this is an Investigator's proposal the Lead has not decided.
    pub fn is_candidate(&self) -> bool {
        matches!(
            self.details.disposition,
            Some(FindingDisposition::Candidate)
        )
    }
}

/// How a child's run ended, in the four readings a parent must tell apart.
///
/// R007b N1: a child died on its wall budget and returned only a stop string.
/// The parent read that as "investigated, nothing to report" and closed the task
/// without ever opening the file the child had been reading. "Finished and found
/// nothing" and "stopped before finding anything" are opposite instructions, and
/// a bare text field cannot carry the difference.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChildStatus {
    /// Ran to a clean end and reported something.
    CompletedWithFindings,
    /// Ran to a clean end with nothing to flag. This IS a result.
    CompletedNoFindings,
    /// Stopped early, but what it had established survives in `findings`.
    IncompletePartial,
    /// Stopped early with nothing to show. NOT the same as "no findings".
    IncompleteNoResult,
}

impl ChildStatus {
    pub fn label(self) -> &'static str {
        match self {
            ChildStatus::CompletedWithFindings => "COMPLETED_WITH_FINDINGS",
            ChildStatus::CompletedNoFindings => "COMPLETED_NO_FINDINGS",
            ChildStatus::IncompletePartial => "INCOMPLETE_PARTIAL",
            ChildStatus::IncompleteNoResult => "INCOMPLETE_NO_RESULT",
        }
    }

    /// Whether the child reached the end of its task.
    pub fn completed(self) -> bool {
        matches!(
            self,
            ChildStatus::CompletedWithFindings | ChildStatus::CompletedNoFindings
        )
    }
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
    /// Whether the child held a physically read-only toolset.
    #[serde(default)]
    pub read_only: bool,
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
            read_only: false,
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
        read_only: bool,
    ) -> Self {
        self.profile_id = Some(profile_id.into());
        self.profile_role = Some(profile_role.into());
        self.read_only = read_only;
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
            details: FindingDetails::default(),
        }
    }

    /// An explorer/worker finding keeps its old wire shape: review-only keys
    /// (including `reproduced: false`) do not appear at all.
    #[test]
    fn ordinary_findings_keep_the_legacy_json_shape() {
        let json = serde_json::to_string(&rec("f-1", "a1")).unwrap();
        for key in [
            "title",
            "dimension",
            "severity",
            "confidence",
            "line",
            "evidence",
            "impact",
            "recommendation",
            "reproduced",
            "disposition",
            "disposition_reason",
            "disposition_evidence",
            "duplicate_of",
            "source_finding_id",
        ] {
            assert!(
                !json.contains(&format!("\"{key}\"")),
                "{key} leaked: {json}"
            );
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
    fn review_fields_round_trip_and_legacy_records_still_decode() {
        let rec = FindingRecord {
            id: "f-1".into(),
            source_child: "review-1".into(),
            role: "reviewer".into(),
            kind: FindingKind::Correctness,
            summary: "missing owner check".into(),
            file: Some("src/auth.rs".into()),
            symbol: None,
            details: FindingDetails {
                title: Some("SQL 缺少 owner 校验".into()),
                dimension: Some(ReviewDimension::Security),
                severity: Some(FindingSeverity::P1),
                confidence: Some(FindingConfidence::High),
                line: Some(42),
                evidence: Some("src/auth.rs:42".into()),
                impact: Some("跨租户读取".into()),
                recommendation: Some("按 owner 过滤".into()),
                reproduced: true,
                ..FindingDetails::default()
            },
        };
        let json = serde_json::to_string(&rec).unwrap();
        assert_eq!(serde_json::from_str::<FindingRecord>(&json).unwrap(), rec);

        // A pre-review finding has none of the new keys and still decodes.
        let legacy = r#"{"id":"f-1","kind":"risk","summary":"old"}"#;
        let decoded: FindingRecord = serde_json::from_str(legacy).unwrap();
        assert_eq!(decoded.details, FindingDetails::default());
    }

    #[test]
    fn review_field_parsers_refuse_unknown_values() {
        assert_eq!(
            ReviewDimension::parse("security"),
            Some(ReviewDimension::Security)
        );
        assert_eq!(ReviewDimension::parse("nonsense"), None);
        assert_eq!(FindingSeverity::parse("P1"), Some(FindingSeverity::P1));
        assert_eq!(FindingSeverity::parse("p9"), None);
        assert_eq!(
            FindingConfidence::parse("low"),
            Some(FindingConfidence::Low)
        );
    }

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
            .with_profile("reviewer", "reviewer", true);
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
        let p: ChildResultProjection =
            serde_json::from_str(r#"{"child_id":"a1","role":"explorer","findings_total":2}"#)
                .unwrap();
        assert_eq!(p.findings_total, 2);
        assert_eq!(p.profile_id, None);
        assert!(
            !p.read_only,
            "absent means not recorded, and false is the floor"
        );
    }

    #[test]
    fn a_structured_verdict_roundtrips_without_being_derived_from_prose() {
        let verdict = ReviewVerdict {
            conclusion: ReviewConclusion::IssuesFound,
            coverage: vec![ReviewCoverage {
                dimension: ReviewDimension::Security,
                status: ReviewCoverageStatus::Partial,
                scope: Some("支付模块".into()),
                evidence_refs: vec!["src/auth.rs:42".into()],
                limitations: Some("无法访问生产配置".into()),
            }],
            limitations: vec![ReviewLimitation {
                kind: ReviewLimitationKind::ToolCapability,
                detail: "没有命令工具，不能运行测试".into(),
            }],
            report: Some("完整正式报告".into()),
            report_revision: Some(1),
            evidence_revision: Some(0),
        };
        let json = serde_json::to_string(&verdict).unwrap();
        assert_eq!(
            serde_json::from_str::<ReviewVerdict>(&json).unwrap(),
            verdict
        );
    }

    #[test]
    fn verdict_parsers_refuse_unknown_values() {
        assert_eq!(
            ReviewCoverageStatus::parse("reviewed"),
            Some(ReviewCoverageStatus::Reviewed)
        );
        assert_eq!(ReviewCoverageStatus::parse("all_good"), None);
        assert_eq!(
            ReviewConclusion::parse("NO_ISSUES_FOUND"),
            Some(ReviewConclusion::NoIssuesFound)
        );
        assert_eq!(ReviewConclusion::parse("passed"), None);
        assert_eq!(
            ReviewLimitationKind::parse("tool"),
            Some(ReviewLimitationKind::ToolCapability)
        );
        assert_eq!(ReviewLimitationKind::parse("vibes"), None);
    }

    /// Only the Lead's own findings and explicitly adopted candidates are
    /// formal. Pending candidates, rejections and merged duplicates never count
    /// as defects.
    #[test]
    fn formal_findings_exclude_candidates_rejections_and_duplicates() {
        let mut formal = rec("f-1", "");
        assert!(formal.is_formal(), "a lead-authored finding is formal");

        formal.details.disposition = Some(FindingDisposition::Candidate);
        assert!(!formal.is_formal() && formal.is_candidate());

        formal.details.disposition = Some(FindingDisposition::Adopted);
        assert!(formal.is_formal());

        formal.details.disposition = Some(FindingDisposition::Rejected);
        assert!(!formal.is_formal());

        formal.details.disposition = Some(FindingDisposition::Duplicate);
        assert!(!formal.is_formal());
    }

    #[test]
    fn a_candidate_keeps_its_source_finding_and_disposition_through_serde() {
        let mut finding = rec("f-2", "agent-1");
        finding.details = FindingDetails {
            disposition: Some(FindingDisposition::Adopted),
            disposition_reason: Some("confirmed against the order state machine".into()),
            disposition_evidence: vec!["src/settle.rs:42".into()],
            source_finding_id: Some("f-1".into()),
            ..FindingDetails::default()
        };
        let json = serde_json::to_string(&finding).unwrap();
        assert_eq!(
            serde_json::from_str::<FindingRecord>(&json).unwrap(),
            finding
        );
        assert_eq!(
            FindingDisposition::parse("unresolved"),
            Some(FindingDisposition::Candidate)
        );
        assert_eq!(FindingDisposition::parse("maybe"), None);
    }
}
