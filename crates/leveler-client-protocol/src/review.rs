//! Manual review: the structured request a client sends and the facts a
//! runtime returns.
//!
//! Review is an explicit, manually started, read-only investigation. Nothing
//! here classifies a user's ordinary message: the only way in is an explicit
//! [`crate::ClientCommand::RunReview`], which carries the scope as data. The
//! reviewer model reads the focus text and decides HOW to investigate; the
//! runtime decides only WHAT object was requested.
//!
//! The report is durable and independent of the session's task terminal: a
//! review never opens a turn, never settles a goal and never edits the
//! workspace.

use leveler_core::{CommandId, SessionId};
use leveler_model::ModelRef;
use serde::{Deserialize, Serialize};

/// What the review is about. The runtime resolves this into concrete files,
/// a diff and an object identity before the reviewer starts.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum UiReviewScope {
    /// The current task's delivery in this session: its user objective, the
    /// files its latest settled delivery touched, and its final claim.
    CurrentTask,
    /// Uncommitted changes in the session workspace.
    Workspace,
    /// The whole project: the reviewer reads the repository itself.
    Project,
    /// The current session, or another stored session named by id.
    Session {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        session_id: Option<SessionId>,
    },
    /// One commit, reviewed from `git show`.
    Commit { rev: String },
    /// The current branch relative to `base` (`git diff base...HEAD`).
    Branch { base: String },
    /// Explicit files, directories or modules.
    Paths { paths: Vec<String> },
}

/// Lifecycle of one manual review. `Completed` says the reviewer reached a
/// clean end; it never means the object is defect-free.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum UiReviewStatus {
    Queued,
    Running,
    Completed,
    /// The reviewer stopped on a budget or an abnormal stop before finishing.
    /// Findings it had established survive.
    Incomplete,
    Cancelled,
    Failed,
}

impl UiReviewStatus {
    pub fn settled(self) -> bool {
        !matches!(self, Self::Queued | Self::Running)
    }

    /// Stable storage/wire label. Unknown values are refused by [`Self::parse`],
    /// never silently coerced.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Queued => "queued",
            Self::Running => "running",
            Self::Completed => "completed",
            Self::Incomplete => "incomplete",
            Self::Cancelled => "cancelled",
            Self::Failed => "failed",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        Some(match value {
            "queued" => Self::Queued,
            "running" => Self::Running,
            "completed" => Self::Completed,
            "incomplete" => Self::Incomplete,
            "cancelled" => Self::Cancelled,
            "failed" => Self::Failed,
            _ => return None,
        })
    }
}

/// Review dimensions. A finding may belong to one dimension; the report also
/// states which dimensions were covered and which were not.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum UiReviewDimension {
    Code,
    Business,
    Security,
    Architecture,
    Test,
    Delivery,
    Session,
}

/// Finding severity. P0 is a confirmed critical defect; P3 is a low-impact
/// observation worth recording.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum UiReviewSeverity {
    P0,
    P1,
    P2,
    P3,
}

/// How sure the reviewer is that the finding is real, distinct from how bad it
/// would be if it were real.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum UiReviewConfidence {
    High,
    Medium,
    Low,
}

/// Counts by severity. `unclassified` is an explicit bucket: a finding without
/// a declared severity is not silently promoted to P2.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct UiReviewCounts {
    pub p0: u32,
    pub p1: u32,
    pub p2: u32,
    pub p3: u32,
    /// Findings recorded without a severity (usually a non-review child).
    pub unclassified: u32,
}

impl UiReviewCounts {
    pub fn total(self) -> u32 {
        self.p0 + self.p1 + self.p2 + self.p3 + self.unclassified
    }
}

/// Model spend attributed to this review, read from the runtime's durable
/// model-request ledger. Unknown cost stays `None`, never zero.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct UiReviewUsage {
    #[serde(default)]
    pub input_tokens: u64,
    #[serde(default)]
    pub output_tokens: u64,
    #[serde(default)]
    pub cached_input_tokens: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cost_usd_micros: Option<u64>,
}

/// The concrete object a review was performed against. Captured before the
/// reviewer starts and re-checked when it ends, so a report can say whether
/// the workspace moved under it.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct UiReviewObject {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace: Option<String>,
    /// `git rev-parse HEAD` at capture time, when the workspace is a Git repo.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub head: Option<String>,
    /// Whether the worktree had uncommitted changes when captured.
    #[serde(default)]
    pub dirty: bool,
    /// Files the requested scope resolved to, repository-relative and capped.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub files: Vec<String>,
    /// New files not tracked by Git at capture time.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub untracked: Vec<String>,
    /// The commit/branch the request named, when it named one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub revision: Option<String>,
    /// SHA-256 over the scope, revision, diff, workspace HEAD/status and a
    /// bounded sample of untracked files. Used only to detect movement; it is
    /// not an identity of the whole repository and a file's middle beyond the
    /// sample is not hashed.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub fingerprint: String,
    /// RFC 3339 capture time.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub captured_at: String,
}

/// One finding in the client-facing report. Stable `id` is the review ledger's
/// own sequence; the report never re-keys another agent's records.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct UiReviewFinding {
    pub id: String,
    /// One-line title. Falls back to `summary` when the reviewer supplied none.
    pub title: String,
    pub summary: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dimension: Option<UiReviewDimension>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub severity: Option<UiReviewSeverity>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub confidence: Option<UiReviewConfidence>,
    /// The `report_finding` kind (`correctness`, `risk`, …).
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub kind: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub file: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub line: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub symbol: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub evidence: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub impact: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub recommendation: Option<String>,
    /// Whether the reviewer reproduced the problem, as opposed to reasoning
    /// from the code alone. `false` is not a claim that it is wrong.
    #[serde(default)]
    pub reproduced: bool,
}

/// The summary a conversation card and history list carry.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct UiReviewSummary {
    pub id: String,
    pub session_id: SessionId,
    /// The user's review focus, verbatim.
    pub focus: String,
    pub scope: UiReviewScope,
    /// The model that performed the review.
    pub model: String,
    pub status: UiReviewStatus,
    pub created_at: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub finished_at: Option<String>,
    pub counts: UiReviewCounts,
    /// The reviewer's own verdict, when it declared one. Absent is "not
    /// declared", never "no issues".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub conclusion: Option<UiReviewConclusion>,
    /// The object moved between capture and the end of the review. A report
    /// that says so is a report about the object as it was, not as it is.
    #[serde(default)]
    pub object_changed: bool,
    /// The reviewer changed product files. The read-only seat makes this a
    /// contract violation, recorded rather than hidden.
    #[serde(default)]
    pub modified_source: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub failure: Option<String>,
    /// Why a non-completed review stopped (budget, cancellation, abnormal
    /// stop), kept separate from `failure`: an incomplete review is not a
    /// failed one, and its partial findings are still evidence.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stop_detail: Option<String>,
    #[serde(default)]
    pub usage: UiReviewUsage,
}

/// How far one dimension was actually covered. `Reviewed` is the reviewer's
/// declaration about its own scope, not a correctness proof.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum UiReviewCoverageStatus {
    NotApplicable,
    NotReviewed,
    Partial,
    Reviewed,
}

impl UiReviewCoverageStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::NotApplicable => "not_applicable",
            Self::NotReviewed => "not_reviewed",
            Self::Partial => "partial",
            Self::Reviewed => "reviewed",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        Some(match value {
            "not_applicable" => Self::NotApplicable,
            "not_reviewed" => Self::NotReviewed,
            "partial" => Self::Partial,
            "reviewed" => Self::Reviewed,
            _ => return None,
        })
    }
}

/// One dimension's declared coverage.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct UiReviewCoverage {
    pub dimension: UiReviewDimension,
    pub status: UiReviewCoverageStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scope: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub evidence_refs: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limitations: Option<String>,
}

/// Where a limitation came from. `Runtime` is a mechanical fact the runtime
/// observed; `Reviewer` is the model's own statement.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum UiReviewLimitationSource {
    Reviewer,
    Runtime,
}

/// What stopped the review from establishing something.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum UiReviewLimitationKind {
    Environment,
    Permission,
    ToolCapability,
    Scope,
    ObjectChanged,
    Other,
}

impl UiReviewLimitationKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Environment => "environment",
            Self::Permission => "permission",
            Self::ToolCapability => "tool_capability",
            Self::Scope => "scope",
            Self::ObjectChanged => "object_changed",
            Self::Other => "other",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        Some(match value {
            "environment" => Self::Environment,
            "permission" => Self::Permission,
            "tool_capability" => Self::ToolCapability,
            "scope" => Self::Scope,
            "object_changed" => Self::ObjectChanged,
            "other" => Self::Other,
            _ => return None,
        })
    }
}

/// One limitation, attributed to whoever observed it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct UiReviewLimitation {
    pub source: UiReviewLimitationSource,
    pub kind: UiReviewLimitationKind,
    pub detail: String,
}

/// The reviewer's own verdict, independent of execution status and counts.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum UiReviewConclusion {
    IssuesFound,
    NoIssuesFound,
    Inconclusive,
}

impl UiReviewConclusion {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::IssuesFound => "issues_found",
            Self::NoIssuesFound => "no_issues_found",
            Self::Inconclusive => "inconclusive",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        Some(match value {
            "issues_found" => Self::IssuesFound,
            "no_issues_found" => Self::NoIssuesFound,
            "inconclusive" => Self::Inconclusive,
            _ => return None,
        })
    }
}

/// Structured coverage, limitations and conclusion. Every collection may be
/// empty; an empty verdict means the reviewer declared nothing.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct UiReviewVerdict {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub conclusion: Option<UiReviewConclusion>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub coverage: Vec<UiReviewCoverage>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub limitations: Vec<UiReviewLimitation>,
}

/// The full report behind a summary card.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct UiReviewReport {
    pub summary: UiReviewSummary,
    pub object: UiReviewObject,
    pub findings: Vec<UiReviewFinding>,
    /// Structured coverage, limitations and conclusion. Absent fields are
    /// "not declared"; the runtime never derives them from the prose below.
    #[serde(default)]
    pub verdict: UiReviewVerdict,
    /// The reviewer's final report, verbatim. The runtime does not parse it
    /// for control flow; coverage, limitations and the conclusion are the
    /// reviewer's own statements and are rendered as written.
    pub report: String,
}

/// Correlation id carried by [`crate::ClientCommand::QueryReviewReport`].
pub type ReviewQueryId = CommandId;

/// A review request as the runtime records it. Not a wire command: the wire
/// command carries the same fields individually so clients can evolve.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReviewRequest {
    pub session_id: SessionId,
    pub focus: String,
    pub scope: UiReviewScope,
    pub model: Option<ModelRef>,
}
