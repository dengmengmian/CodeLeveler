//! Durable finding lifecycle (multi-agent product closure).
//!
//! One typed record for what a child (explorer / worker / reviewer) or the
//! parent established, with an explicit audited state machine. The records
//! live in [`crate::EvidenceLedger`] so they persist and replay through the
//! existing `EvidenceLedgerUpdated` events — no second persistence system.

use serde::{Deserialize, Serialize};

/// Where a finding is in its life. Small and closed on purpose.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FindingState {
    /// Reported at the source (a child's own ledger).
    Created,
    /// Received by the parent. Receipt is not judgment.
    Acknowledged,
    /// The parent judged it relevant.
    Accepted,
    /// The parent explicitly declined it, with a reason. Terminal.
    Rejected,
    /// Work was done for it, but not yet proven.
    Addressed,
    /// Proven by fresh post-mutation verification. Terminal.
    Verified,
}

impl FindingState {
    pub fn label(self) -> &'static str {
        match self {
            Self::Created => "created",
            Self::Acknowledged => "acknowledged",
            Self::Accepted => "accepted",
            Self::Rejected => "rejected",
            Self::Addressed => "addressed",
            Self::Verified => "verified",
        }
    }
}

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
    /// A blocking finding not Rejected/Verified prevents a verified closure.
    #[serde(default)]
    pub blocking: bool,
    pub state: FindingState,
    /// Required when `state == Rejected`; explains the parent's judgment.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resolution_reason: Option<String>,
}

impl FindingRecord {
    /// A finding that still stands in the way of a verified closure.
    pub fn open_blocking(&self) -> bool {
        self.blocking && !matches!(self.state, FindingState::Rejected | FindingState::Verified)
    }
}

/// Why a finding transition was refused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FindingError {
    UnknownId(String),
    IllegalTransition {
        from: FindingState,
        to: FindingState,
    },
    RejectNeedsReason,
}

impl std::fmt::Display for FindingError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::UnknownId(id) => write!(f, "no finding with id `{id}`"),
            Self::IllegalTransition { from, to } => write!(
                f,
                "a finding cannot go from `{}` to `{}`",
                from.label(),
                to.label()
            ),
            Self::RejectNeedsReason => write!(f, "rejecting a finding requires a reason"),
        }
    }
}

impl std::error::Error for FindingError {}

/// The audited state machine. Everything not listed is illegal — in
/// particular `Created → Verified` cannot happen by construction.
pub fn transition_allowed(from: FindingState, to: FindingState) -> bool {
    use FindingState::*;
    matches!(
        (from, to),
        (Created, Acknowledged)
            | (Acknowledged, Accepted)
            | (Acknowledged, Rejected)
            | (Accepted, Addressed)
            | (Accepted, Rejected)
            | (Addressed, Verified)
    )
}

/// A compact, durable view of what one child produced — enough to trace its
/// contribution without copying its findings into an event.
///
/// The terminal event `SubAgentFinished` used to carry a prose preview and
/// nothing else, so replaying a log told you a child finished and roughly what
/// it said, never what it found in a form anything could join on. The finding
/// records existed the whole time (490 of them across twenty MA-VALUE-A runs)
/// with `source_child` on every one; nothing connected them to the outcome.
///
/// This is deliberately counts and a reference, not the records themselves.
/// Embedding payloads in events is what the event pipeline just finished paying
/// down — the authority stays in the ledger, which is already persisted.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct ChildResultProjection {
    /// The child this projects. Joins to `FindingRecord::source_child`.
    pub child_id: String,
    pub role: String,
    /// Findings in the parent ledger attributed to this child.
    pub findings_total: u32,
    /// Reached the parent at all — every state except `Created`, which means
    /// the record never left the child's own ledger.
    pub findings_acknowledged: u32,
    /// The parent judged it relevant (`Accepted`, `Addressed` or `Verified`).
    pub findings_accepted: u32,
    /// Proven by fresh post-mutation verification.
    pub findings_verified: u32,
    /// Declined with a reason. Counted because a rejection IS a contribution
    /// — the parent looked and decided — and a rate that ignores it would
    /// reward children whose findings are merely never judged.
    pub findings_rejected: u32,
    /// Still blocking a verified closure.
    pub findings_open_blocking: u32,
}

impl ChildResultProjection {
    /// Project one child's contribution out of the parent's ledger findings.
    ///
    /// Pure: takes the records, returns counts. The caller owns where the
    /// records come from, so this is testable without a running agent.
    pub fn from_findings(child_id: &str, role: &str, findings: &[FindingRecord]) -> Self {
        let mine: Vec<&FindingRecord> = findings
            .iter()
            .filter(|f| f.source_child == child_id)
            .collect();
        Self {
            child_id: child_id.to_string(),
            role: role.to_string(),
            findings_total: mine.len() as u32,
            findings_acknowledged: mine
                .iter()
                .filter(|f| !matches!(f.state, FindingState::Created))
                .count() as u32,
            findings_accepted: mine
                .iter()
                .filter(|f| {
                    matches!(
                        f.state,
                        FindingState::Accepted | FindingState::Addressed | FindingState::Verified
                    )
                })
                .count() as u32,
            findings_verified: mine
                .iter()
                .filter(|f| matches!(f.state, FindingState::Verified))
                .count() as u32,
            findings_rejected: mine
                .iter()
                .filter(|f| matches!(f.state, FindingState::Rejected))
                .count() as u32,
            findings_open_blocking: mine.iter().filter(|f| f.open_blocking()).count() as u32,
        }
    }

    /// Did the parent act on anything this child reported?
    ///
    /// A rejection counts: the parent read it and made a call. What does not
    /// count is a finding nobody ever judged.
    pub fn contributed(&self) -> bool {
        self.findings_accepted > 0 || self.findings_rejected > 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_lifecycle_admits_exactly_the_documented_transitions() {
        use FindingState::*;
        let legal = [
            (Created, Acknowledged),
            (Acknowledged, Accepted),
            (Acknowledged, Rejected),
            (Accepted, Addressed),
            (Accepted, Rejected),
            (Addressed, Verified),
        ];
        let all = [
            Created,
            Acknowledged,
            Accepted,
            Rejected,
            Addressed,
            Verified,
        ];
        for from in all {
            for to in all {
                assert_eq!(
                    transition_allowed(from, to),
                    legal.contains(&(from, to)),
                    "{} -> {}",
                    from.label(),
                    to.label()
                );
            }
        }
    }

    /// The audit jump the design forbids by name.
    #[test]
    fn created_cannot_jump_to_verified() {
        assert!(!transition_allowed(
            FindingState::Created,
            FindingState::Verified
        ));
    }

    #[test]
    fn terminal_states_admit_nothing() {
        use FindingState::*;
        for terminal in [Rejected, Verified] {
            for to in [
                Created,
                Acknowledged,
                Accepted,
                Rejected,
                Addressed,
                Verified,
            ] {
                assert!(!transition_allowed(terminal, to));
            }
        }
    }

    #[test]
    fn kinds_round_trip_between_parse_and_label() {
        for raw in [
            "relevant_file",
            "relevant_symbol",
            "dependency",
            "callsite",
            "risk",
            "test",
            "config",
            "observation",
            "correctness",
        ] {
            let kind = FindingKind::parse(raw).unwrap_or_else(|| panic!("{raw} must parse"));
            assert_eq!(kind.label(), raw);
        }
        assert_eq!(
            FindingKind::parse("vibe"),
            None,
            "unknown kinds are refused"
        );
    }

    #[test]
    fn open_blocking_reads_the_two_terminal_states_as_settled() {
        let mut rec = FindingRecord {
            id: "f-1".into(),
            source_child: "agent-1".into(),
            role: "reviewer".into(),
            kind: FindingKind::Correctness,
            summary: "off-by-one".into(),
            file: None,
            symbol: None,
            blocking: true,
            state: FindingState::Acknowledged,
            resolution_reason: None,
        };
        assert!(rec.open_blocking());
        rec.state = FindingState::Addressed;
        assert!(rec.open_blocking(), "addressed-but-unproven still blocks");
        rec.state = FindingState::Verified;
        assert!(!rec.open_blocking());
        rec.state = FindingState::Rejected;
        assert!(!rec.open_blocking());
        rec.state = FindingState::Accepted;
        rec.blocking = false;
        assert!(!rec.open_blocking(), "non-blocking never blocks");
    }

    /// Replay compatibility: a ledger snapshot serialized before findings
    /// existed must still deserialize (serde defaults), and a snapshot with
    /// findings must round-trip every field.
    #[test]
    fn records_round_trip_through_serde() {
        let rec = FindingRecord {
            id: "f-2".into(),
            source_child: "agent-2".into(),
            role: "explorer".into(),
            kind: FindingKind::RelevantFile,
            summary: "config loader lives here".into(),
            file: Some("src/config.rs".into()),
            symbol: Some("load".into()),
            blocking: false,
            state: FindingState::Created,
            resolution_reason: None,
        };
        let json = serde_json::to_string(&rec).unwrap();
        assert!(json.contains("\"relevant_file\""), "{json}");
        assert!(json.contains("\"created\""), "{json}");
        let back: FindingRecord = serde_json::from_str(&json).unwrap();
        assert_eq!(back, rec);
    }
    fn rec(id: &str, child: &str, state: FindingState, blocking: bool) -> FindingRecord {
        FindingRecord {
            id: id.into(),
            source_child: child.into(),
            role: "explorer".into(),
            kind: FindingKind::Observation,
            summary: "s".into(),
            file: None,
            symbol: None,
            blocking,
            state,
            resolution_reason: None,
        }
    }

    /// The projection answers "which child produced this, and did the parent
    /// act on it" from records alone — the join MA-VALUE-A could not make.
    #[test]
    fn a_projection_counts_only_its_own_child() {
        let all = vec![
            rec("f1", "agent-1", FindingState::Accepted, false),
            rec("f2", "agent-1", FindingState::Verified, false),
            rec("f3", "agent-2", FindingState::Accepted, false),
            rec("f4", "agent-1", FindingState::Created, false),
        ];
        let p = ChildResultProjection::from_findings("agent-1", "explorer", &all);
        assert_eq!(p.findings_total, 3, "agent-2's finding is not agent-1's");
        assert_eq!(
            p.findings_acknowledged, 2,
            "a Created record never left the child's own ledger"
        );
        assert_eq!(p.findings_accepted, 2, "Accepted and Verified both count");
        assert_eq!(p.findings_verified, 1);
    }

    /// A rejection IS a contribution: the parent read the finding and made a
    /// call. Counting only acceptances would reward a child whose findings are
    /// never judged over one whose findings are judged and declined.
    #[test]
    fn a_rejected_finding_still_counts_as_contribution() {
        let all = vec![rec("f1", "agent-1", FindingState::Rejected, false)];
        let p = ChildResultProjection::from_findings("agent-1", "explorer", &all);
        assert_eq!(p.findings_rejected, 1);
        assert_eq!(p.findings_accepted, 0);
        assert!(
            p.contributed(),
            "the parent looked and decided — that is contribution"
        );
    }

    /// A child whose findings were never judged has not contributed yet, and
    /// must not be scored as if it had.
    #[test]
    fn findings_nobody_judged_are_not_contribution() {
        let all = vec![
            rec("f1", "agent-1", FindingState::Created, false),
            rec("f2", "agent-1", FindingState::Acknowledged, false),
        ];
        let p = ChildResultProjection::from_findings("agent-1", "explorer", &all);
        assert_eq!(p.findings_total, 2);
        assert_eq!(p.findings_acknowledged, 1);
        assert!(
            !p.contributed(),
            "receipt is not judgment — acknowledged alone is not contribution"
        );
    }

    /// An open blocking finding is what stops a verified closure, so it is
    /// counted separately from the accept/reject question.
    #[test]
    fn open_blocking_findings_are_surfaced_separately() {
        let all = vec![
            rec("f1", "agent-1", FindingState::Acknowledged, true),
            rec("f2", "agent-1", FindingState::Rejected, true),
        ];
        let p = ChildResultProjection::from_findings("agent-1", "explorer", &all);
        assert_eq!(
            p.findings_open_blocking, 1,
            "a rejected blocking finding no longer blocks"
        );
    }

    /// A child that reported nothing projects cleanly rather than panicking or
    /// looking like a child that was never asked.
    #[test]
    fn a_child_with_no_findings_projects_zeros() {
        let p = ChildResultProjection::from_findings("agent-9", "explorer", &[]);
        assert_eq!(p.findings_total, 0);
        assert!(!p.contributed());
        assert_eq!(p.child_id, "agent-9");
    }

    /// The projection rides events and must survive replay.
    #[test]
    fn a_projection_roundtrips_through_json() {
        let p = ChildResultProjection::from_findings(
            "agent-1",
            "reviewer",
            &[rec("f1", "agent-1", FindingState::Verified, false)],
        );
        let json = serde_json::to_string(&p).unwrap();
        let back: ChildResultProjection = serde_json::from_str(&json).unwrap();
        assert_eq!(back, p);
    }
}
