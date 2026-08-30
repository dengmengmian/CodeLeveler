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
    /// Durable ids this evidence points at — the tool calls that made the
    /// change or ran the check. A claim that cites nothing resolvable is
    /// prose, and prose is not a binding: the runtime resolves these against
    /// the ledger rather than taking the description's word for it.
    #[serde(default)]
    pub refs: Vec<String>,
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
    /// What proof this obligation demands. Fixed when the obligation is
    /// derived — before the work starts and before anyone knows what evidence
    /// will happen to exist — so it cannot be relaxed to fit what turned up.
    #[serde(default)]
    pub evidence_policy: Option<EvidencePolicy>,
    pub evidence: Vec<RequirementEvidence>,
}

impl CompletionRequirement {
    fn has_mechanical_evidence(&self) -> bool {
        self.evidence
            .iter()
            .any(|e| e.strength == EvidenceStrength::Mechanical)
    }
}

/// What proof an obligation demands.
///
/// The kind says what is owed; this says how it may be proven. Two obligations
/// that are both `Verification` can need completely different evidence, and
/// conflating them cost one failure in each direction: "`go test ./...` must
/// pass" was refused while four green runs sat in the ledger, and "the boundary
/// rule is covered by a test" would be discharged by a pre-existing suite going
/// green over code that added no test at all.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EvidencePolicy {
    /// Named commands must have run and succeeded. The runtime answers this
    /// from its own record; no reading of the work is involved.
    CommandSuccess {
        /// Normalized command fingerprints, matched against the ledger.
        commands: Vec<String>,
        mode: CommandMode,
    },
    /// A concrete test must address the obligation AND have been exercised.
    /// Neither half is enough: a suite going green says nothing about whether
    /// the required case exists, and a cited test that never ran proves
    /// nothing about whether it passes.
    TestCoverage,
    /// The proof standard could not be determined. Fails closed — guessing
    /// "any green check" here is exactly how a missing test gets waved
    /// through.
    Unresolved,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CommandMode {
    /// Every named command must have succeeded — the default for "X and Y
    /// must pass", where satisfying one is not satisfying the obligation.
    All,
    /// Any one of them suffices, and only when the wording actually says so.
    Any,
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
        match r.kind {
            // The proof standard was fixed when the obligation was derived.
            // Asking the judge's label first was an inversion that cost a
            // correct run — four green checks sat in the ledger while the
            // obligation was refused because the judge had described them in
            // prose. The model may say what evidence means; it does not decide
            // whether the runtime looks.
            RequirementKind::Verification => match r.evidence_policy.as_ref() {
                Some(EvidencePolicy::CommandSuccess { commands, mode }) => {
                    let satisfied = |c: &String| ledger.fresh_successful_command(c);
                    match mode {
                        CommandMode::All => !commands.is_empty() && commands.iter().all(satisfied),
                        CommandMode::Any => commands.iter().any(satisfied),
                    }
                }
                // Both halves, or neither counts. A binding that resolves to
                // nothing is a sentence, and a green suite over code that
                // added no test says nothing about the case that was asked for.
                Some(EvidencePolicy::TestCoverage) => {
                    r.evidence
                        .iter()
                        .flat_map(|e| e.refs.iter())
                        .any(|id| ledger.resolves_evidence_ref(id))
                        && ledger.has_fresh_successful_verify()
                }
                // No policy is not a licence. Guessing "any green check" here
                // is precisely how a missing test gets waved through.
                Some(EvidencePolicy::Unresolved) | None => false,
            },
            // Unchanged: nothing changed means nothing was delivered, and the
            // judge must still have claimed mechanical backing. What proves a
            // specific deliverable exists is its own question.
            RequirementKind::Deliverable => {
                r.has_mechanical_evidence() && !ledger.mutations.is_empty()
            }
            // Not everything a user asks for is machine-checkable.
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
            evidence_policy: None,
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
            refs: Vec::new(),
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
            refs: Vec::new(),
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
            refs: Vec::new(),
        });
        // Under V1 the proof standard is explicit: this obligation names a
        // command, so that command having run green is what settles it.
        r.evidence_policy = Some(EvidencePolicy::CommandSuccess {
            commands: vec!["go\u{1f}test".into()],
            mode: CommandMode::All,
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
            refs: Vec::new(),
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
            refs: Vec::new(),
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

#[cfg(test)]
mod evidence_policy_tests {
    use super::*;
    use crate::EvidenceLedger;

    const BUILD: &str = "go\u{1f}build\u{1f}./...";
    const TEST: &str = "go\u{1f}test\u{1f}./...";

    fn requirement(
        text: &str,
        policy: EvidencePolicy,
        evidence: Vec<RequirementEvidence>,
    ) -> CompletionRequirement {
        CompletionRequirement {
            id: "R6".into(),
            text: text.into(),
            kind: RequirementKind::Verification,
            source: RequirementSource::OriginalGoal,
            status: RequirementStatus::Satisfied,
            evidence_policy: Some(policy),
            evidence,
        }
    }

    fn prose(strength: EvidenceStrength) -> Vec<RequirementEvidence> {
        vec![RequirementEvidence {
            strength,
            detail: "verification is recorded as green".into(),
            refs: Vec::new(),
        }]
    }

    fn cited(strength: EvidenceStrength, refs: &[&str]) -> Vec<RequirementEvidence> {
        vec![RequirementEvidence {
            strength,
            detail: "the regression test for this behaviour".into(),
            refs: refs.iter().map(|r| r.to_string()).collect(),
        }]
    }

    fn debt(r: CompletionRequirement, mut ledger: EvidenceLedger) -> Option<String> {
        ledger.completion_contract = Some(CompletionContract::new(vec![r]));
        ledger.completion_debt()
    }

    fn edited() -> EvidenceLedger {
        let mut l = EvidenceLedger::default();
        l.record_mutation(
            "c1",
            "apply_patch",
            vec!["internal/report/summary.go".into()],
        );
        l
    }

    // ── CommandSuccess: the HC-002 direction ────────────────────────────

    /// The obligation named two commands and both ran green over the current
    /// tree. That the judge described them in prose is beside the point —
    /// this is a question about the record, and the record answers it.
    #[test]
    fn named_commands_that_ran_green_discharge_the_obligation() {
        let mut ledger = edited();
        ledger.record_verify("v1", BUILD, 0);
        ledger.record_verify("v2", TEST, 0);
        let r = requirement(
            "`go build ./...` and `go test ./...` must pass.",
            EvidencePolicy::CommandSuccess {
                commands: vec![BUILD.into(), TEST.into()],
                mode: CommandMode::All,
            },
            prose(EvidenceStrength::Semantic),
        );
        assert_eq!(debt(r, ledger), None);
    }

    /// "X and Y must pass" is not satisfied by Y alone.
    #[test]
    fn one_of_two_required_commands_is_not_enough() {
        let mut ledger = edited();
        ledger.record_verify("v2", TEST, 0);
        let r = requirement(
            "both must pass",
            EvidencePolicy::CommandSuccess {
                commands: vec![BUILD.into(), TEST.into()],
                mode: CommandMode::All,
            },
            prose(EvidenceStrength::Mechanical),
        );
        assert!(debt(r, ledger).is_some());
    }

    /// A different command going green is not the named one going green.
    #[test]
    fn a_different_command_does_not_discharge_the_named_one() {
        let mut ledger = edited();
        ledger.record_verify("v1", "go\u{1f}test\u{1f}./pkg/other", 0);
        let r = requirement(
            "`go test ./...` must pass",
            EvidencePolicy::CommandSuccess {
                commands: vec![TEST.into()],
                mode: CommandMode::All,
            },
            prose(EvidenceStrength::Mechanical),
        );
        assert!(debt(r, ledger).is_some());
    }

    /// A run invalidated by a later edit proves nothing about what shipped.
    #[test]
    fn a_stale_command_result_does_not_discharge() {
        let mut ledger = edited();
        ledger.record_verify("v1", TEST, 0);
        ledger.record_mutation("c2", "apply_patch", vec!["b.go".into()]);
        let r = requirement(
            "`go test ./...` must pass",
            EvidencePolicy::CommandSuccess {
                commands: vec![TEST.into()],
                mode: CommandMode::All,
            },
            prose(EvidenceStrength::Mechanical),
        );
        assert!(debt(r, ledger).is_some());
    }

    #[test]
    fn a_failing_command_does_not_discharge() {
        let mut ledger = edited();
        ledger.record_verify("v1", TEST, 1);
        let r = requirement(
            "`go test ./...` must pass",
            EvidencePolicy::CommandSuccess {
                commands: vec![TEST.into()],
                mode: CommandMode::All,
            },
            prose(EvidenceStrength::Mechanical),
        );
        assert!(debt(r, ledger).is_some());
    }

    /// Calling prose "mechanical" does not make a command have run.
    #[test]
    fn a_mechanical_label_cannot_fabricate_a_command_result() {
        let r = requirement(
            "`go test ./...` must pass",
            EvidencePolicy::CommandSuccess {
                commands: vec![TEST.into()],
                mode: CommandMode::All,
            },
            prose(EvidenceStrength::Mechanical),
        );
        assert!(debt(r, edited()).is_some());
    }

    // ── TestCoverage: the scale-s800 direction ──────────────────────────

    /// THE scale-s800 false completion. Production code changed, no test was
    /// written, and the pre-existing suite went green. A suite passing says
    /// nothing about whether the case the user asked for is covered.
    #[test]
    fn a_green_suite_alone_does_not_discharge_a_coverage_obligation() {
        let mut ledger = edited();
        ledger.record_verify("v1", TEST, 0);
        let r = requirement(
            "the boundary rule is covered by a test",
            EvidencePolicy::TestCoverage,
            prose(EvidenceStrength::Mechanical),
        );
        assert!(
            debt(r, ledger).is_some(),
            "a green suite is not the required test"
        );
    }

    /// A cited test that actually exists, plus a green run over the current
    /// tree, discharges it. Both halves are needed and both are present.
    #[test]
    fn a_cited_test_that_ran_green_discharges_the_coverage_obligation() {
        let mut ledger = EvidenceLedger::default();
        ledger.record_mutation(
            "c9",
            "apply_patch",
            vec!["internal/window/window_test.go".into()],
        );
        ledger.record_verify("v1", TEST, 0);
        let r = requirement(
            "the boundary rule is covered by a test",
            EvidencePolicy::TestCoverage,
            cited(EvidenceStrength::Semantic, &["c9"]),
        );
        assert_eq!(debt(r, ledger), None);
    }

    /// An existing test the agent did not have to write is citable too: the
    /// binding is to the run that exercised it, not to a file having changed.
    #[test]
    fn an_existing_test_can_be_cited_through_the_run_that_exercised_it() {
        let mut ledger = edited();
        ledger.record_verify(
            "v7",
            "go\u{1f}test\u{1f}./internal/report\u{1f}-run\u{1f}TestBoundary",
            0,
        );
        ledger.record_verify("v8", TEST, 0);
        let r = requirement(
            "the boundary rule is covered by a test",
            EvidencePolicy::TestCoverage,
            cited(EvidenceStrength::Observed, &["v7"]),
        );
        assert_eq!(debt(r, ledger), None);
    }

    /// A citation to something that never happened is prose with a reference
    /// number on it.
    #[test]
    fn an_unresolvable_citation_does_not_discharge() {
        let mut ledger = edited();
        ledger.record_verify("v1", TEST, 0);
        let r = requirement(
            "the boundary rule is covered by a test",
            EvidencePolicy::TestCoverage,
            cited(EvidenceStrength::Mechanical, &["c-never-happened"]),
        );
        assert!(debt(r, ledger).is_some());
    }

    /// Cited test, but nothing ran it since the last edit.
    #[test]
    fn a_cited_test_that_never_ran_does_not_discharge() {
        let mut ledger = EvidenceLedger::default();
        ledger.record_mutation("c9", "apply_patch", vec!["window_test.go".into()]);
        let r = requirement(
            "the boundary rule is covered by a test",
            EvidencePolicy::TestCoverage,
            cited(EvidenceStrength::Mechanical, &["c9"]),
        );
        assert!(debt(r, ledger).is_some());
    }

    /// Cited test, ran green, then the code changed again.
    #[test]
    fn a_cited_test_with_a_stale_run_does_not_discharge() {
        let mut ledger = EvidenceLedger::default();
        ledger.record_mutation("c9", "apply_patch", vec!["window_test.go".into()]);
        ledger.record_verify("v1", TEST, 0);
        ledger.record_mutation("c10", "apply_patch", vec!["window.go".into()]);
        let r = requirement(
            "the boundary rule is covered by a test",
            EvidencePolicy::TestCoverage,
            cited(EvidenceStrength::Mechanical, &["c9"]),
        );
        assert!(debt(r, ledger).is_some());
    }

    // ── policy integrity ────────────────────────────────────────────────

    /// An obligation whose proof standard could not be determined fails
    /// closed. Falling back to "any green check" is how the coverage case got
    /// waved through in the first place.
    #[test]
    fn an_unresolved_policy_never_discharges() {
        let mut ledger = edited();
        ledger.record_verify("v1", TEST, 0);
        let unresolved = requirement(
            "something verifiable",
            EvidencePolicy::Unresolved,
            prose(EvidenceStrength::Mechanical),
        );
        assert!(debt(unresolved, ledger.clone()).is_some());
        let mut missing = requirement(
            "something verifiable",
            EvidencePolicy::TestCoverage,
            prose(EvidenceStrength::Mechanical),
        );
        missing.evidence_policy = None;
        assert!(
            debt(missing, ledger).is_some(),
            "no policy is not a licence"
        );
    }

    /// A coverage obligation does not become a command obligation because the
    /// only evidence that turned up was a green suite.
    #[test]
    fn a_coverage_obligation_is_not_downgraded_by_the_evidence_that_exists() {
        let mut ledger = edited();
        ledger.record_verify("v1", BUILD, 0);
        ledger.record_verify("v2", TEST, 0);
        let r = requirement(
            "the boundary rule is covered by a test",
            EvidencePolicy::TestCoverage,
            prose(EvidenceStrength::Mechanical),
        );
        assert!(
            debt(r, ledger).is_some(),
            "every command in the world going green is still not the test"
        );
    }

    /// Requirements without a mechanical predicate keep their semantics.
    #[test]
    fn semantic_requirements_are_unchanged() {
        let r = CompletionRequirement {
            id: "R1".into(),
            text: "the summary reads clearly".into(),
            kind: RequirementKind::Behavior,
            source: RequirementSource::OriginalGoal,
            status: RequirementStatus::Satisfied,
            evidence_policy: None,
            evidence: prose(EvidenceStrength::Semantic),
        };
        assert_eq!(debt(r, EvidenceLedger::default()), None);
    }

    /// The policy rides the durable contract, so a resumed run cannot end up
    /// with a weaker proof standard than the one it started under.
    #[test]
    fn the_policy_survives_serialization() {
        let r = requirement(
            "`go test ./...` must pass",
            EvidencePolicy::CommandSuccess {
                commands: vec![TEST.into()],
                mode: CommandMode::All,
            },
            cited(EvidenceStrength::Mechanical, &["v1"]),
        );
        let contract = CompletionContract::new(vec![r]);
        let round: CompletionContract =
            serde_json::from_str(&serde_json::to_string(&contract).unwrap()).unwrap();
        assert_eq!(round, contract);
    }
}
