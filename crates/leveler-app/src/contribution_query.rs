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
    read_only: bool,
) -> UiChildContribution {
    let Some(ledger) = ledger else {
        return UiChildContribution {
            child_id: child_id.to_string(),
            role: role.to_string(),
            profile_id,
            read_only,
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
        read_only,
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

/// Role, profile and write bound for one child, from its spawn event.
///
/// These are the child's own facts, not ledger facts. Reading them from the
/// event rather than inferring from the id keeps the inspector honest about a
/// child whose profile was never recorded: `None` means "not recorded".
pub async fn child_identity(
    events: &dyn leveler_storage::EventStore,
    session_id: &SessionId,
    child_id: &str,
) -> (String, Option<String>, bool) {
    let Ok(rows) = events.load(session_id).await else {
        return (String::new(), None, false);
    };
    for row in rows {
        if row.event_type != "sub_agent_started" {
            continue;
        }
        if let Ok(leveler_engine::EngineEvent::SubAgentStarted {
            id,
            role,
            profile_id,
            read_only,
            ..
        }) = leveler_engine::EngineEvent::from_payload(&row.payload)
            && id == child_id
        {
            return (role, profile_id, read_only);
        }
    }
    (String::new(), None, false)
}

#[cfg(test)]
mod tests {
    use super::*;
    use leveler_lifecycle::{FindingKind, FindingRecord};

    fn rec(id: &str, child: &str) -> FindingRecord {
        FindingRecord {
            id: id.into(),
            source_child: child.into(),
            role: "reviewer".into(),
            kind: FindingKind::Correctness,
            summary: format!("summary {id}"),
            file: Some("src/auth.rs".into()),
            symbol: None,
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
        let l = ledger(vec![rec("f-1", "a1"), rec("f-2", "a2"), rec("f-3", "a1")]);
        let got = project_child_contribution(Some(&l), "a1", "reviewer", None, true);
        assert!(got.measured);
        assert_eq!(got.findings.len(), 2);
        assert_eq!(got.findings[0].id, "f-1");
        assert_eq!(got.findings[0].summary, "summary f-1");
        assert_eq!(got.findings[0].file.as_deref(), Some("src/auth.rs"));
    }

    /// "No ledger" and "found nothing" are different answers and must not
    /// render alike: the first is unmeasured, the second is a clean review.
    #[test]
    fn an_absent_ledger_is_unmeasured_not_empty() {
        let none = project_child_contribution(None, "a1", "reviewer", None, true);
        assert!(!none.measured);
        assert!(none.findings.is_empty());

        let empty =
            project_child_contribution(Some(&ledger(Vec::new())), "a1", "reviewer", None, true);
        assert!(empty.measured);
        assert!(empty.reviewed_clean());
    }

    #[test]
    fn the_returned_list_is_bounded() {
        let many: Vec<FindingRecord> = (0..(CONTRIBUTION_FINDINGS_MAX + 10))
            .map(|i| rec(&format!("f-{i}"), "a1"))
            .collect();
        let got = project_child_contribution(Some(&ledger(many)), "a1", "reviewer", None, true);
        assert_eq!(got.findings.len(), CONTRIBUTION_FINDINGS_MAX);
    }
}
