//! The Completion Contract: material obligations derived from the ORIGINAL
//! goal, carried for the whole task, and reconciled item by item before any
//! completion is allowed.
//!
//! Completion Truth used to be decided by one reading at the very end: the
//! model re-read the goal, re-read the evidence, and said satisfied. That
//! reading is where two observed defects live. In `icg-6r` the executor
//! discovered the objective was unsatisfiable, quietly reinterpreted it, and
//! completed the reinterpretation. In `scale-s800` the goal said in so many
//! words that "the boundary rule is covered by a test", the implementation
//! landed without one, and the final reading called it satisfied anyway.
//!
//! Both are the same shape: the requirement stopped existing at the moment it
//! was inconvenient. So the contract makes requirements durable — derived once
//! at the start of the goal, before the executor has met any obstacle, and
//! never rewritten by the executor's own later description of the task.
//!
//! The contract does NOT replace the goal. [`ObjectiveAnchor`] stays
//! authoritative: a requirement missing from this ledger does not make the
//! user's ask disappear, it only means the mechanical accounting cannot see
//! it, and the semantic gate still runs behind this one.
//!
//! [`ObjectiveAnchor`]: crate::ObjectiveAnchor

use serde::{Deserialize, Serialize};

use crate::EvidenceLedger;

/// What kind of obligation this is. The kind decides whether the mechanical
/// floor applies — not everything a user asks for is machine-checkable, and
/// pretending otherwise would either block honest work or invite a heuristic
/// that guesses which command proves which sentence.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RequirementKind {
    /// Observable behaviour of the thing being built.
    Behavior,
    /// An obligation to DEMONSTRATE something — "covered by a test", "verify
    /// it builds". Mechanically floored: prose cannot discharge it.
    Verification,
    /// A boundary on how the work may be done ("do not change existing
    /// tests", "only touch the windowing package").
    Constraint,
    /// Something that must exist when the work is done. Mechanically floored:
    /// nothing in the tree changed means nothing was delivered.
    Deliverable,
    Other,
}

impl RequirementKind {
    /// Whether a claim of satisfaction must be backed by a recorded fact.
    fn demands_mechanical_evidence(self) -> bool {
        matches!(
            self,
            RequirementKind::Verification | RequirementKind::Deliverable
        )
    }
}

/// Where the obligation came from. Only the user can create one: the executor
/// cannot mint requirements for itself, and — more importantly — cannot retire
/// one by restating the task in easier words.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RequirementSource {
    OriginalGoal,
    ExplicitUserFollowup,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RequirementStatus {
    Pending,
    Satisfied,
    /// Honestly unreachable. Blocked keeps completion shut just as firmly as
    /// pending — it is a truthful ending, not a discharged obligation.
    Blocked,
}

/// How strong a piece of evidence is. Mechanical beats observed beats
/// semantic; the ordering exists so that what CAN be proven is not waved
/// through on a feeling.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EvidenceStrength {
    /// The model's reading of what happened.
    Semantic,
    /// A recorded observation — output that was actually seen.
    Observed,
    /// A recorded fact in the ledger: a check that ran, a file that changed.
    Mechanical,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RequirementEvidence {
    pub strength: EvidenceStrength,
    pub detail: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CompletionRequirement {
    pub id: String,
    /// The obligation in the ORIGINAL wording as far as possible. This text is
    /// what the user asked for; it is not a summary of what got built.
    pub text: String,
    pub kind: RequirementKind,
    pub source: RequirementSource,
    pub status: RequirementStatus,
    pub evidence: Vec<RequirementEvidence>,
}

impl CompletionRequirement {
    fn has_mechanical_evidence(&self) -> bool {
        self.evidence
            .iter()
            .any(|e| e.strength == EvidenceStrength::Mechanical)
    }
}

/// Why an obligation is still open. The executor needs to know which of these
/// it is facing — finish the work, prove it, or stop and say it cannot be
/// done — and the completion report counts them separately.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OpenReason {
    /// Nobody accounted for it, or it was accounted for as unsatisfied.
    NotAccountedFor,
    /// Honestly unreachable. Truthful, and still not a completion.
    Blocked,
    /// Claimed satisfied, but a demonstrable obligation with nothing that
    /// demonstrates it over the tree as it stands.
    MissingMechanicalEvidence,
}

/// The material obligations of one goal, derived once and carried for its
/// lifetime.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CompletionContract {
    pub requirements: Vec<CompletionRequirement>,
}

impl CompletionContract {
    pub fn new(requirements: Vec<CompletionRequirement>) -> Self {
        Self { requirements }
    }

    /// Whether this contract carries anything at all. An empty contract is
    /// silence, not approval: derivation may have failed, and the caller must
    /// fall back to the semantic gate rather than read nothing as consent.
    pub fn accounts_for_anything(&self) -> bool {
        !self.requirements.is_empty()
    }

    /// Every obligation that cannot be discharged on the evidence recorded so
    /// far. Completion is allowed only when this is empty.
    ///
    /// Two ways to stay on this list:
    ///
    /// 1. the obligation is not marked satisfied — pending or honestly
    ///    blocked, both keep the gate shut;
    /// 2. it is marked satisfied, it is the kind of obligation that can be
    ///    demonstrated, and nothing in the ledger demonstrates it.
    ///
    /// The second rule is the mechanical floor. It is what "the boundary rule
    /// is covered by a test" needed: a judgment of satisfied, unaccompanied by
    /// a check that ran green over the tree as it stands, is not enough.
    /// How many obligations are demands to DEMONSTRATE something.
    pub fn verification_count(&self) -> usize {
        self.requirements
            .iter()
            .filter(|r| r.kind == RequirementKind::Verification)
            .count()
    }

    /// How many obligations the run reported as honestly unreachable.
    pub fn blocked_count(&self) -> usize {
        self.requirements
            .iter()
            .filter(|r| r.status == RequirementStatus::Blocked)
            .count()
    }

    /// Every open obligation with the reason it is open.
    pub fn open_obligations(
        &self,
        ledger: &EvidenceLedger,
    ) -> Vec<(&CompletionRequirement, OpenReason)> {
        self.requirements
            .iter()
            .filter_map(|r| self.open_reason(r, ledger).map(|why| (r, why)))
            .collect()
    }

    fn open_reason(
        &self,
        r: &CompletionRequirement,
        ledger: &EvidenceLedger,
    ) -> Option<OpenReason> {
        match r.status {
            RequirementStatus::Blocked => Some(OpenReason::Blocked),
            RequirementStatus::Pending => Some(OpenReason::NotAccountedFor),
            RequirementStatus::Satisfied if !self.is_discharged(r, ledger) => {
                Some(OpenReason::MissingMechanicalEvidence)
            }
            RequirementStatus::Satisfied => None,
        }
    }

    pub fn unsatisfied_material(&self, ledger: &EvidenceLedger) -> Vec<&CompletionRequirement> {
        self.requirements
            .iter()
            .filter(|r| !self.is_discharged(r, ledger))
            .collect()
    }

    fn is_discharged(&self, r: &CompletionRequirement, ledger: &EvidenceLedger) -> bool {
        if r.status != RequirementStatus::Satisfied {
            return false;
        }
        if !r.kind.demands_mechanical_evidence() {
            return true;
        }
        if !r.has_mechanical_evidence() {
            return false;
        }
        match r.kind {
            // A demonstration is only a demonstration over the tree that
            // exists now: a green check followed by more edits proves nothing
            // about what those edits did.
            RequirementKind::Verification => ledger.has_fresh_successful_verify(),
            // Nothing changed means nothing was delivered.
            RequirementKind::Deliverable => !ledger.mutations.is_empty(),
            _ => true,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::EvidenceLedger;

    fn requirement(id: &str, kind: RequirementKind) -> CompletionRequirement {
        CompletionRequirement {
            id: id.to_string(),
            text: format!("requirement {id}"),
            kind,
            source: RequirementSource::OriginalGoal,
            status: RequirementStatus::Pending,
            evidence: Vec::new(),
        }
    }

    /// A requirement nobody accounted for blocks completion. This is the whole
    /// point: `scale-s800`'s test obligation could not be reached by the final
    /// reading, so it had to still be sitting here, unsatisfied.
    #[test]
    fn a_pending_requirement_is_unsatisfied() {
        let contract = CompletionContract::new(vec![requirement("r1", RequirementKind::Behavior)]);
        let ledger = EvidenceLedger::default();
        let open = contract.unsatisfied_material(&ledger);
        assert_eq!(open.len(), 1);
        assert_eq!(open[0].id, "r1");
    }

    /// A behaviour requirement the judge marked satisfied needs no mechanical
    /// fact of its own — not everything a user asks for is machine-checkable,
    /// and the goal is not to turn the agent into a theorem prover.
    #[test]
    fn a_satisfied_behaviour_requirement_clears() {
        let mut r = requirement("r1", RequirementKind::Behavior);
        r.status = RequirementStatus::Satisfied;
        r.evidence.push(RequirementEvidence {
            strength: EvidenceStrength::Semantic,
            detail: "the summary no longer prints invalid rows".into(),
        });
        let contract = CompletionContract::new(vec![r]);
        assert!(
            contract
                .unsatisfied_material(&EvidenceLedger::default())
                .is_empty()
        );
    }

    /// THE MECHANICAL FLOOR. A verification obligation ("covered by a test")
    /// that the judge calls satisfied, with nothing but prose behind it, does
    /// not clear. "I added the test" is a claim about the world, and the
    /// ledger is the world.
    #[test]
    fn a_verification_requirement_needs_a_mechanical_fact_not_prose() {
        let mut r = requirement("r2", RequirementKind::Verification);
        r.status = RequirementStatus::Satisfied;
        r.evidence.push(RequirementEvidence {
            strength: EvidenceStrength::Semantic,
            detail: "I added a test for the boundary rule".into(),
        });
        let contract = CompletionContract::new(vec![r]);
        let open = contract.unsatisfied_material(&EvidenceLedger::default());
        assert_eq!(
            open.len(),
            1,
            "a verification obligation cannot be discharged by saying so"
        );
    }

    /// The same obligation clears once a check actually ran green over the
    /// changed tree.
    #[test]
    fn a_verification_requirement_clears_on_a_fresh_green_check() {
        let mut r = requirement("r2", RequirementKind::Verification);
        r.status = RequirementStatus::Satisfied;
        r.evidence.push(RequirementEvidence {
            strength: EvidenceStrength::Mechanical,
            detail: "go test ./...".into(),
        });
        let contract = CompletionContract::new(vec![r]);
        let mut ledger = EvidenceLedger::default();
        ledger.record_mutation(
            "c1",
            "apply_patch",
            vec!["internal/window/window_test.go".into()],
        );
        ledger.record_verify("v1", "go\u{1f}test", 0);
        assert!(contract.unsatisfied_material(&ledger).is_empty());
    }

    /// A check that went green and was then invalidated by a later edit is not
    /// evidence for anything: the tree it passed over no longer exists.
    #[test]
    fn a_stale_check_does_not_discharge_a_verification_requirement() {
        let mut r = requirement("r2", RequirementKind::Verification);
        r.status = RequirementStatus::Satisfied;
        r.evidence.push(RequirementEvidence {
            strength: EvidenceStrength::Mechanical,
            detail: "go test ./...".into(),
        });
        let contract = CompletionContract::new(vec![r]);
        let mut ledger = EvidenceLedger::default();
        ledger.record_mutation("c1", "apply_patch", vec!["a.go".into()]);
        ledger.record_verify("v1", "go\u{1f}test", 0);
        ledger.record_mutation("c2", "apply_patch", vec!["b.go".into()]);
        assert_eq!(
            contract.unsatisfied_material(&ledger).len(),
            1,
            "a check invalidated by a later edit proves nothing about the tree now"
        );
    }

    /// A deliverable that nothing in the tree reflects has not been delivered.
    #[test]
    fn a_deliverable_requirement_needs_a_mutation() {
        let mut r = requirement("r3", RequirementKind::Deliverable);
        r.status = RequirementStatus::Satisfied;
        let contract = CompletionContract::new(vec![r]);
        assert_eq!(
            contract
                .unsatisfied_material(&EvidenceLedger::default())
                .len(),
            1
        );
    }

    /// Blocked is an honest state, not a satisfied one — it must keep the
    /// completion shut, which is exactly what `icg-6r` needs.
    #[test]
    fn a_blocked_requirement_never_clears() {
        let mut r = requirement("r1", RequirementKind::Behavior);
        r.status = RequirementStatus::Blocked;
        let contract = CompletionContract::new(vec![r]);
        assert_eq!(
            contract
                .unsatisfied_material(&EvidenceLedger::default())
                .len(),
            1
        );
    }

    /// The gate has to say WHY, not just that something is open: "you never
    /// accounted for it", "you claimed it without a check", and "you told me it
    /// cannot be done" are three different situations for the executor, and
    /// three different numbers in the report.
    #[test]
    fn each_open_obligation_reports_why_it_is_open() {
        let mut pending = requirement("r1", RequirementKind::Behavior);
        pending.status = RequirementStatus::Pending;

        let mut blocked = requirement("r2", RequirementKind::Behavior);
        blocked.status = RequirementStatus::Blocked;

        let mut unproven = requirement("r3", RequirementKind::Verification);
        unproven.status = RequirementStatus::Satisfied;
        unproven.evidence.push(RequirementEvidence {
            strength: EvidenceStrength::Semantic,
            detail: "I added a test".into(),
        });

        let contract = CompletionContract::new(vec![pending, blocked, unproven]);
        let open = contract.open_obligations(&EvidenceLedger::default());
        assert_eq!(open.len(), 3);
        assert_eq!(open[0].1, OpenReason::NotAccountedFor);
        assert_eq!(open[1].1, OpenReason::Blocked);
        assert_eq!(open[2].1, OpenReason::MissingMechanicalEvidence);
    }

    /// The counts the report needs, straight off the contract.
    #[test]
    fn the_contract_counts_its_own_shape() {
        let mut blocked = requirement("r2", RequirementKind::Behavior);
        blocked.status = RequirementStatus::Blocked;
        let contract = CompletionContract::new(vec![
            requirement("r1", RequirementKind::Verification),
            blocked,
            requirement("r3", RequirementKind::Behavior),
        ]);
        assert_eq!(contract.requirements.len(), 3);
        assert_eq!(contract.verification_count(), 1);
        assert_eq!(contract.blocked_count(), 1);
    }

    /// An empty contract is not a licence to complete — it means derivation
    /// produced nothing, and the caller must fall back to the semantic gate
    /// rather than read silence as approval.
    #[test]
    fn an_empty_contract_reports_that_it_accounts_for_nothing() {
        assert!(!CompletionContract::new(Vec::new()).accounts_for_anything());
        assert!(
            CompletionContract::new(vec![requirement("r1", RequirementKind::Behavior)])
                .accounts_for_anything()
        );
    }
}
