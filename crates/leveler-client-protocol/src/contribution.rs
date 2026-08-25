//! Read model for the Contribution Inspector.
//!
//! Answers "what did this child actually contribute, and what became of it" on
//! demand, rather than pushing every finding through the event stream.
//!
//! The split is deliberate and matches the split the event-pipeline work
//! settled on:
//!
//! ```text
//! EventLog  →  what happened, in order
//! Ledger    →  what is true now
//! ```
//!
//! Findings live in the ledger. Streaming them as events would put the same
//! record in two places and make the event stream carry payloads it was just
//! trimmed of. The inspector queries instead — the user opens a detail view,
//! the client asks, the runtime answers from the ledger.

use serde::{Deserialize, Serialize};

/// One finding, as the inspector shows it.
///
/// A projection of `leveler_lifecycle::FindingRecord`, not the record itself:
/// this crate is the stable wire, and the ledger must stay free to change.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct UiFinding {
    /// Parent-ledger id (`f-1`). Stable enough for a user to refer to.
    pub id: String,
    /// `relevant_file`, `risk`, `correctness`, …
    pub kind: String,
    pub summary: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub file: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub symbol: Option<String>,
    /// `created` | `acknowledged` | `accepted` | `rejected` | `addressed` |
    /// `verified`.
    pub state: String,
    /// Why the parent declined it. Present only on a rejection, where it is
    /// required — a rejection without a reason is not a judgement.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resolution_reason: Option<String>,
    /// Still gates a verified closure.
    pub blocking: bool,
}

/// Everything the inspector shows for one child.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct UiChildContribution {
    pub child_id: String,
    pub role: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub profile_id: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub capabilities: Vec<String>,
    /// Findings this child produced, in ledger order.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub findings: Vec<UiFinding>,
    /// Whether a ledger snapshot was found at all.
    ///
    /// `false` means the question could not be answered — no ledger, or the
    /// child predates finding adoption. It does NOT mean the child found
    /// nothing, and the inspector must not render it that way.
    pub measured: bool,
}

impl UiChildContribution {
    /// The child was measured and reported nothing. A result, not an empty
    /// state, and the single most common Reviewer outcome.
    pub fn reviewed_clean(&self) -> bool {
        self.measured && self.findings.is_empty()
    }

    pub fn accepted(&self) -> usize {
        self.findings
            .iter()
            .filter(|f| matches!(f.state.as_str(), "accepted" | "addressed" | "verified"))
            .count()
    }

    pub fn verified(&self) -> usize {
        self.findings.iter().filter(|f| f.state == "verified").count()
    }

    pub fn rejected(&self) -> usize {
        self.findings.iter().filter(|f| f.state == "rejected").count()
    }

    /// Reported, and nobody ever judged it. The protocol's definition of noise.
    pub fn unjudged(&self) -> usize {
        self.findings
            .iter()
            .filter(|f| matches!(f.state.as_str(), "created" | "acknowledged"))
            .count()
    }
}

/// How many findings one query returns. A child that somehow produced more has
/// its list truncated rather than the response growing without bound; the
/// counts on `ChildResultProjection` remain the authority for totals.
pub const CONTRIBUTION_FINDINGS_MAX: usize = 200;
