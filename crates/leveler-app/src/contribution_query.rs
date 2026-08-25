//! Contribution Inspector: read one child's findings out of the ledger.
//!
//! Read-only and on demand. Findings are ledger facts, so the inspector reads
//! the last persisted `EvidenceLedgerUpdated` snapshot rather than having the
//! runtime stream every finding as an event — which would duplicate the record
//! and re-grow the event payloads the pipeline work trimmed.

use leveler_client_protocol::{CONTRIBUTION_FINDINGS_MAX, UiChildContribution, UiFinding};
use leveler_core::SessionId;
use leveler_lifecycle::{EvidenceLedger, FindingRecord};

/// Project one child's findings out of a ledger snapshot.
///
/// Pure: takes the ledger, returns the read model. The caller owns where the
/// ledger came from, so this is testable without a database.
///
/// `ledger == None` means no snapshot exists — the question is unanswerable,
/// which `measured: false` says. It is not the same as a child that found
/// nothing, and the two must not render alike.
pub fn project_child_contribution(
    ledger: Option<&EvidenceLedger>,
    child_id: &str,
    role: &str,
    profile_id: Option<String>,
    capabilities: Vec<String>,
) -> UiChildContribution {
    let Some(ledger) = ledger else {
        return UiChildContribution {
            child_id: child_id.to_string(),
            role: role.to_string(),
            profile_id,
            capabilities,
            findings: Vec::new(),
            measured: false,
        };
    };
    let findings: Vec<UiFinding> = ledger
        .findings
        .iter()
        .filter(|f| f.source_child == child_id)
        .take(CONTRIBUTION_FINDINGS_MAX)
        .map(project_finding)
        .collect();
    UiChildContribution {
        child_id: child_id.to_string(),
        role: role.to_string(),
        profile_id,
        capabilities,
        findings,
        measured: true,
    }
}

fn project_finding(f: &FindingRecord) -> UiFinding {
    UiFinding {
        id: f.id.clone(),
        kind: f.kind.label().to_string(),
        summary: f.summary.clone(),
        file: f.file.clone(),
        symbol: f.symbol.clone(),
        state: f.state.label().to_string(),
        resolution_reason: f.resolution_reason.clone(),
        blocking: f.blocking,
    }
}

/// The last persisted ledger snapshot for one session, or `None` when the
/// session never wrote one.
pub async fn last_ledger(
    events: &dyn leveler_storage::EventStore,
    session_id: &SessionId,
) -> Option<EvidenceLedger> {
    let rows = events.load(session_id).await.ok()?;
    let mut out = None;
    for row in rows {
        if row.event_type == "evidence_ledger_updated"
            && let Ok(leveler_engine::EngineEvent::EvidenceLedgerUpdated { ledger }) =
                leveler_engine::EngineEvent::from_payload(&row.payload)
        {
            out = Some(ledger);
        }
    }
    out
}

/// Role, profile and capabilities for one child, from its spawn event.
///
/// These are the child's own facts, not ledger facts. Reading them from the
/// event rather than inferring from the id keeps the inspector honest about a
/// child whose profile was never recorded: empty means "not recorded".
pub async fn child_identity(
    events: &dyn leveler_storage::EventStore,
    session_id: &SessionId,
    child_id: &str,
) -> (String, Option<String>, Vec<String>) {
    let Ok(rows) = events.load(session_id).await else {
        return (String::new(), None, Vec::new());
    };
    for row in rows {
        if row.event_type != "sub_agent_started" {
            continue;
        }
        if let Ok(leveler_engine::EngineEvent::SubAgentStarted {
            id,
            role,
            profile_id,
            capabilities,
            ..
        }) = leveler_engine::EngineEvent::from_payload(&row.payload)
            && id == child_id
        {
            return (role, profile_id, capabilities);
        }
    }
    (String::new(), None, Vec::new())
}

#[cfg(test)]
mod tests {
    use super::*;
    use leveler_lifecycle::{FindingKind, FindingState};

    fn rec(id: &str, child: &str, state: FindingState, blocking: bool) -> FindingRecord {
        FindingRecord {
            id: id.into(),
            source_child: child.into(),
            role: "reviewer".into(),
            kind: FindingKind::Correctness,
            summary: format!("finding {id}"),
            file: Some("src/auth.rs".into()),
            symbol: None,
            blocking,
            state,
            resolution_reason: if matches!(state, FindingState::Rejected) {
                Some("covered by the existing guard".into())
            } else {
                None
            },
        }
    }

    fn ledger(findings: Vec<FindingRecord>) -> EvidenceLedger {
        EvidenceLedger {
            findings,
            ..Default::default()
        }
    }

    #[test]
    fn a_child_sees_only_its_own_findings() {
        let l = ledger(vec![
            rec("f-1", "a1", FindingState::Accepted, false),
            rec("f-2", "a2", FindingState::Accepted, false),
        ]);
        let got = project_child_contribution(Some(&l), "a1", "explorer", None, Vec::new());
        assert_eq!(got.findings.len(), 1);
        assert_eq!(got.findings[0].id, "f-1");
    }

    #[test]
    fn no_ledger_is_unmeasured_not_clean() {
        let got = project_child_contribution(None, "a1", "reviewer", None, Vec::new());
        assert!(!got.measured);
        assert!(
            !got.reviewed_clean(),
            "an unanswerable question must not read as a clean review"
        );
    }

    #[test]
    fn a_measured_child_with_no_findings_is_a_clean_review() {
        let l = ledger(vec![rec("f-1", "other", FindingState::Accepted, false)]);
        let got = project_child_contribution(Some(&l), "r1", "reviewer", None, Vec::new());
        assert!(got.measured);
        assert!(got.reviewed_clean());
    }

    #[test]
    fn lifecycle_states_are_counted_by_what_the_parent_did() {
        let l = ledger(vec![
            rec("f-1", "r1", FindingState::Verified, false),
            rec("f-2", "r1", FindingState::Accepted, false),
            rec("f-3", "r1", FindingState::Rejected, false),
            rec("f-4", "r1", FindingState::Acknowledged, false),
        ]);
        let got = project_child_contribution(Some(&l), "r1", "reviewer", None, Vec::new());
        assert_eq!(got.accepted(), 2, "verified counts as accepted");
        assert_eq!(got.verified(), 1);
        assert_eq!(got.rejected(), 1);
        assert_eq!(got.unjudged(), 1, "acknowledged but never judged");
    }

    #[test]
    fn a_rejection_carries_its_reason() {
        let l = ledger(vec![rec("f-1", "r1", FindingState::Rejected, false)]);
        let got = project_child_contribution(Some(&l), "r1", "reviewer", None, Vec::new());
        assert_eq!(
            got.findings[0].resolution_reason.as_deref(),
            Some("covered by the existing guard"),
            "a rejection without a reason is not a judgement"
        );
    }

    #[test]
    fn the_capability_contract_travels_with_the_detail() {
        let l = ledger(Vec::new());
        let got = project_child_contribution(
            Some(&l),
            "r1",
            "reviewer",
            Some("reviewer".into()),
            vec!["code_review".into()],
        );
        assert_eq!(got.profile_id.as_deref(), Some("reviewer"));
        assert_eq!(got.capabilities, vec!["code_review"]);
    }

    #[test]
    fn the_findings_list_is_bounded() {
        let many: Vec<_> = (0..CONTRIBUTION_FINDINGS_MAX + 50)
            .map(|i| rec(&format!("f-{i}"), "r1", FindingState::Accepted, false))
            .collect();
        let got = project_child_contribution(Some(&ledger(many)), "r1", "reviewer", None, Vec::new());
        assert_eq!(got.findings.len(), CONTRIBUTION_FINDINGS_MAX);
    }

    #[test]
    fn blocking_is_preserved_so_the_inspector_can_mark_it() {
        let l = ledger(vec![rec("f-1", "r1", FindingState::Acknowledged, true)]);
        let got = project_child_contribution(Some(&l), "r1", "reviewer", None, Vec::new());
        assert!(got.findings[0].blocking);
    }
}
