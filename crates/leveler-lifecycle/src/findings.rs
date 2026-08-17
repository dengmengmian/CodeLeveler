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
        let all = [Created, Acknowledged, Accepted, Rejected, Addressed, Verified];
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
            for to in [Created, Acknowledged, Accepted, Rejected, Addressed, Verified] {
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
        assert_eq!(FindingKind::parse("vibe"), None, "unknown kinds are refused");
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
}
