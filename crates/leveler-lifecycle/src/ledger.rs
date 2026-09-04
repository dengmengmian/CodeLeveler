//! Process evidence ledger (Delivery). Pure types — no I/O, no shell.
//!
//! Event log remains SoT for resume; this is the host in-memory projection
//! the readiness gate reads during a drive.

use serde::{Deserialize, Serialize};

use crate::findings::{FindingError, FindingKind, FindingRecord, FindingState, transition_allowed};
use crate::plan::PlanState;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MutationRecord {
    pub seq: u64,
    pub tool_call_id: String,
    pub tool: String,
    pub paths: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VerifyRecord {
    pub seq: u64,
    pub tool_call_id: String,
    /// Normalized `program + args` fingerprint for acceptance matching.
    pub command_fingerprint: String,
    pub exit_code: i32,
    /// Mutation seq observed when this verify ran (invalidate if later mutations).
    pub after_mutation_seq: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CompleteStepReceipt {
    pub step_id: String,
    pub step_text: String,
    pub summary: String,
    /// Must match a successful VerifyRecord.tool_call_id when delivery_gate.
    pub evidence_ref: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InterceptRecord {
    pub kind: String,
    pub detail: String,
}

/// In-memory process evidence for Gate / Delivery.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct EvidenceLedger {
    pub plan: PlanState,
    pub mutations: Vec<MutationRecord>,
    pub verifications: Vec<VerifyRecord>,
    pub step_receipts: Vec<CompleteStepReceipt>,
    pub intercepts: Vec<InterceptRecord>,
    pub next_seq: u64,
    /// Every successful mutating tool call, INCLUDING repeat edits of files
    /// already in the modified set. `mutations` records only first-touch paths
    /// (its gating semantics are unchanged); this counter exists because R011
    /// showed that refinement — fixing files you already wrote — was invisible
    /// to every ledger, so the window judge starved it of progress credit.
    #[serde(default)]
    pub total_mutation_ops: u64,
    /// Durable multi-agent findings (self-reported and adopted from children).
    /// Serde-default so pre-findings snapshots still replay.
    #[serde(default)]
    pub findings: Vec<FindingRecord>,
    /// Whether the runtime has finished producing its own evidence for this
    /// completion attempt.
    ///
    /// The completion contract is asked twice on the same ledger, and the two
    /// moments know different amounts. `update_goal(complete)` asks from
    /// inside the agent loop — the REQUEST — while the runtime's verification
    /// plan runs afterwards and records what it saw; the terminal boundary
    /// then asks again — the COMMIT. One predicate, one authority, two points
    /// on a ledger that grew in between.
    ///
    /// This flag is what lets the predicate tell them apart. Set once the
    /// verification plan's observations are on the ledger. A rule that needs
    /// runtime evidence stays silent while this is false, because refusing
    /// there asks for proof that does not exist yet — the mistake F7-B made
    /// and had to withdraw.
    ///
    /// Serde-default so snapshots written before it existed replay as "still
    /// gathering", which is the conservative reading: they are judged exactly
    /// as they were.
    #[serde(default)]
    pub runtime_evidence_complete: bool,
    /// Monotonic id source for findings owned by THIS ledger.
    #[serde(default)]
    pub next_finding_seq: u64,
    /// The goal's Completion Contract, once derived. It rides the ledger
    /// because the ledger is already the durable, replayed record of what this
    /// run knows — a restart that lost the obligations would be free to
    /// reinterpret the goal from scratch, which is the failure the contract
    /// exists to prevent. Serde-default so pre-contract snapshots still replay.
    #[serde(default)]
    pub completion_contract: Option<crate::CompletionContract>,
    /// A goal ran, and its obligations could not be established. Recorded so
    /// the terminal boundary can tell "no contract was needed" (a chat turn)
    /// apart from "the contract could not be built" (a goal whose obligations
    /// are unknown). The second must never terminate as success: nothing was
    /// checked, and nothing checked is not everything satisfied.
    #[serde(default)]
    pub completion_contract_unavailable: bool,
}

impl EvidenceLedger {
    /// One successful mutating tool call happened (new paths or a re-edit).
    pub fn note_mutation_op(&mut self) {
        self.total_mutation_ops = self.total_mutation_ops.saturating_add(1);
    }

    pub fn last_mutation_seq(&self) -> u64 {
        self.mutations.last().map(|m| m.seq).unwrap_or(0)
    }

    pub fn record_mutation(
        &mut self,
        tool_call_id: impl Into<String>,
        tool: impl Into<String>,
        paths: Vec<String>,
    ) {
        self.next_seq = self.next_seq.saturating_add(1);
        self.mutations.push(MutationRecord {
            seq: self.next_seq,
            tool_call_id: tool_call_id.into(),
            tool: tool.into(),
            paths,
        });
    }

    pub fn record_verify(
        &mut self,
        tool_call_id: impl Into<String>,
        command_fingerprint: impl Into<String>,
        exit_code: i32,
    ) {
        self.next_seq = self.next_seq.saturating_add(1);
        self.verifications.push(VerifyRecord {
            seq: self.next_seq,
            tool_call_id: tool_call_id.into(),
            command_fingerprint: command_fingerprint.into(),
            exit_code,
            after_mutation_seq: self.last_mutation_seq(),
        });
    }

    /// Successful verify that is still valid after the latest mutation.
    /// Did this exact command run green over the tree as it stands?
    ///
    /// Matched on the normalized fingerprint the ledger already records, so
    /// "`go test ./...` must pass" is not satisfied by some other command
    /// that happened to succeed. The runtime answers this from its own
    /// record; no reading of anyone's description is involved.
    pub fn fresh_successful_command(&self, fingerprint: &str) -> bool {
        let last_mut = self.last_mutation_seq();
        self.verifications.iter().any(|v| {
            v.exit_code == 0
                && v.after_mutation_seq >= last_mut
                && last_mut > 0
                && v.command_fingerprint == fingerprint
        })
    }

    /// Does this durable id name something that actually happened?
    ///
    /// Evidence that cites nothing resolvable is prose wearing a citation's
    /// clothes, so a binding is checked against the record rather than
    /// believed.
    pub fn resolves_evidence_ref(&self, id: &str) -> bool {
        self.mutations.iter().any(|m| m.tool_call_id == id)
            || self.verifications.iter().any(|v| v.tool_call_id == id)
    }

    /// Whether `id` names an OBSERVATION this ledger holds: a command that
    /// ran green over code this run had already changed.
    ///
    /// The distinction a citation needs. `resolves_evidence_ref` answers "does
    /// the runtime know this id at all" — F3's rule, which an invented id
    /// fails. This answers "can this id witness what the code does", which a
    /// file edit fails: a path changing is a fact about the tree, not about
    /// its behaviour. A failing run is not a witness either, nor is one that
    /// ran before anything changed — that says what the code already did, not
    /// what the work achieved.
    ///
    /// F7-B: the test is "after ANY mutation", not "at or after the LAST
    /// one". The stricter form discarded a check that had genuinely observed
    /// the work as soon as any later write landed — a harness sanity file was
    /// enough — and it blocked four obligations of an externally accepted
    /// run. Whether the newest edit has been re-checked is a different
    /// question, and [`Self::has_fresh_successful_verify`] is where it is
    /// asked.
    pub fn witnesses_behavior(&self, id: &str) -> bool {
        self.verifications
            .iter()
            .any(|v| v.tool_call_id == id && v.exit_code == 0 && v.after_mutation_seq > 0)
    }

    // `observed_the_changed_tree()` lived here: "did this run ever see a green
    // check over code it had changed", and it was the floor under a behavioural
    // claim the judge could not anchor to an id.
    //
    // It is gone because that question cannot carry that weight. It is true of
    // a run whose only check was `/usr/bin/true`, and true of a run whose green
    // command exercised a neighbouring behaviour — which is the F7 substitution
    // arriving through the floor rather than through a citation. Whether the
    // run observed ANYTHING is not evidence about a PARTICULAR obligation, and
    // the predicate now asks for a proof standard instead
    // (`completion_contract::authoritative_proof_holds`).
    //
    // Deleted rather than left unused: an authority with no callers is an
    // invitation to wire the same hole back in.

    pub fn has_fresh_successful_verify(&self) -> bool {
        let last_mut = self.last_mutation_seq();
        self.verifications
            .iter()
            .any(|v| v.exit_code == 0 && v.after_mutation_seq >= last_mut && last_mut > 0)
    }

    /// Verifications that PASSED before this task changed anything.
    ///
    /// R007b N2: on a bug-fix goal the agent added a reproduction, watched it
    /// go green on the untouched tree, and concluded the defect did not
    /// exist — then drifted to an unrelated fix. A check that passes before
    /// any mutation has demonstrated that the current code satisfies it; on a
    /// fix goal that is the *opposite* of reproducing the bug, so it must
    /// never be presented as proof that the work is done.
    ///
    /// This is evidence semantics, not a policy: nothing is blocked, the
    /// verification is simply not counted as reproduction proof.
    pub fn baseline_green_verifications(&self) -> Vec<&VerifyRecord> {
        self.verifications
            .iter()
            .filter(|v| v.exit_code == 0 && v.after_mutation_seq == 0)
            .collect()
    }

    /// Whether every successful verification so far ran on an unmodified
    /// tree — i.e. nothing has been proven about a change that was never made.
    pub fn only_baseline_green_evidence(&self) -> bool {
        let successful: Vec<_> = self
            .verifications
            .iter()
            .filter(|v| v.exit_code == 0)
            .collect();
        !successful.is_empty() && successful.iter().all(|v| v.after_mutation_seq == 0)
    }

    pub fn find_successful_verify(&self, evidence_ref: &str) -> Option<&VerifyRecord> {
        self.verifications
            .iter()
            .find(|v| v.tool_call_id == evidence_ref && v.exit_code == 0)
    }

    /// Verify is still valid relative to current last mutation.
    pub fn evidence_ref_is_fresh(&self, evidence_ref: &str) -> bool {
        let last_mut = self.last_mutation_seq();
        self.find_successful_verify(evidence_ref)
            .is_some_and(|v| v.after_mutation_seq >= last_mut)
    }

    pub fn record_step_receipt(&mut self, receipt: CompleteStepReceipt) {
        self.step_receipts.push(receipt);
    }

    pub fn record_intercept(&mut self, kind: impl Into<String>, detail: impl Into<String>) {
        self.intercepts.push(InterceptRecord {
            kind: kind.into(),
            detail: detail.into(),
        });
    }

    /// Record a finding this agent itself established (state `Created`).
    /// Returns the assigned id.
    pub fn record_finding(
        &mut self,
        kind: FindingKind,
        summary: impl Into<String>,
        file: Option<String>,
        symbol: Option<String>,
        blocking: bool,
    ) -> String {
        self.next_finding_seq = self.next_finding_seq.saturating_add(1);
        let id = format!("f-{}", self.next_finding_seq);
        self.findings.push(FindingRecord {
            id: id.clone(),
            source_child: String::new(),
            role: String::new(),
            kind,
            summary: summary.into(),
            file,
            symbol,
            blocking,
            state: FindingState::Created,
            resolution_reason: None,
        });
        id
    }

    /// Adopt a child's finding into this (parent) ledger. Adoption re-keys the
    /// id and lands the record at `Acknowledged` — receipt is not judgment.
    /// Returns the parent-side id.
    pub fn adopt_finding(&mut self, source_child: &str, role: &str, rec: &FindingRecord) -> String {
        self.next_finding_seq = self.next_finding_seq.saturating_add(1);
        let id = format!("f-{}", self.next_finding_seq);
        self.findings.push(FindingRecord {
            id: id.clone(),
            source_child: source_child.to_string(),
            role: role.to_string(),
            state: FindingState::Acknowledged,
            ..rec.clone()
        });
        id
    }

    /// Apply a parent judgment to one finding. `Rejected` requires a reason;
    /// `Addressed` is host-promoted straight on to `Verified` when fresh
    /// post-mutation verification already exists. Returns the final state.
    pub fn resolve_finding(
        &mut self,
        id: &str,
        to: FindingState,
        reason: Option<&str>,
        has_fresh_verify: bool,
    ) -> Result<FindingState, FindingError> {
        let rec = self
            .findings
            .iter_mut()
            .find(|f| f.id == id)
            .ok_or_else(|| FindingError::UnknownId(id.to_string()))?;
        if !transition_allowed(rec.state, to) {
            return Err(FindingError::IllegalTransition {
                from: rec.state,
                to,
            });
        }
        if to == FindingState::Rejected {
            let reason = reason.map(str::trim).filter(|r| !r.is_empty());
            let Some(reason) = reason else {
                return Err(FindingError::RejectNeedsReason);
            };
            rec.resolution_reason = Some(reason.to_string());
        }
        rec.state = to;
        // Host promotion, never a model claim: an addressed finding becomes
        // verified only on the strength of fresh post-mutation verification.
        if to == FindingState::Addressed && has_fresh_verify {
            rec.state = FindingState::Verified;
        }
        Ok(rec.state)
    }

    /// Promote every `Addressed` finding to `Verified` once fresh
    /// post-mutation verification exists. Returns how many were promoted.
    pub fn promote_addressed_findings(&mut self, has_fresh_verify: bool) -> usize {
        if !has_fresh_verify {
            return 0;
        }
        let mut promoted = 0;
        for rec in &mut self.findings {
            if rec.state == FindingState::Addressed {
                rec.state = FindingState::Verified;
                promoted += 1;
            }
        }
        promoted
    }

    /// Record a host-authored finding on the parent ledger (not a child's
    /// `report_finding`). Lands at `Acknowledged` — receipt is still not
    /// judgment. Used when a Worker fails to finish scoped work: the parent
    /// must settle this before a verified closure.
    pub fn record_parent_finding(
        &mut self,
        source_child: &str,
        role: &str,
        kind: FindingKind,
        summary: impl Into<String>,
        blocking: bool,
    ) -> String {
        self.next_finding_seq = self.next_finding_seq.saturating_add(1);
        let id = format!("f-{}", self.next_finding_seq);
        self.findings.push(FindingRecord {
            id: id.clone(),
            source_child: source_child.to_string(),
            role: role.to_string(),
            kind,
            summary: summary.into(),
            file: None,
            symbol: None,
            blocking,
            state: FindingState::Acknowledged,
            resolution_reason: None,
        });
        id
    }

    /// Findings that still stand in the way of a verified closure.
    pub fn open_blocking_findings(&self) -> Vec<&FindingRecord> {
        self.findings.iter().filter(|f| f.open_blocking()).collect()
    }

    /// A fresh-epoch ledger carrying ONLY this ledger's unsettled findings
    /// (session review debt), never its mutation/verification evidence — a new
    /// epoch must prove its own work (N2). Settled findings (Rejected /
    /// Verified) stay in history; the id sequence is preserved so carried and
    /// new findings can never collide. `None` when there is nothing to carry.
    pub fn carry_forward_findings(&self) -> Option<EvidenceLedger> {
        let open: Vec<FindingRecord> = self
            .findings
            .iter()
            .filter(|f| !matches!(f.state, FindingState::Rejected | FindingState::Verified))
            .cloned()
            .collect();
        if open.is_empty() {
            return None;
        }
        Some(EvidenceLedger {
            findings: open,
            next_finding_seq: self.next_finding_seq,
            ..EvidenceLedger::default()
        })
    }

    pub fn finding(&self, id: &str) -> Option<&FindingRecord> {
        self.findings.iter().find(|f| f.id == id)
    }

    pub fn normalize_command_fingerprint(program: &str, args: &[String]) -> String {
        let mut parts = vec![program.trim().to_string()];
        parts.extend(args.iter().map(|a| a.trim().to_string()));
        parts.join("\u{1f}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mutation_invalidates_prior_verify() {
        let mut led = EvidenceLedger::default();
        led.record_mutation("c1", "apply_patch", vec!["a.rs".into()]);
        led.record_verify("v1", "cargo\u{1f}test", 0);
        assert!(led.has_fresh_successful_verify());
        led.record_mutation("c2", "replace", vec!["a.rs".into()]);
        assert!(!led.has_fresh_successful_verify());
        assert!(!led.evidence_ref_is_fresh("v1"));
        led.record_verify("v2", "cargo\u{1f}test", 0);
        assert!(led.has_fresh_successful_verify());
        assert!(led.evidence_ref_is_fresh("v2"));
    }

    #[test]
    fn step_receipt_records_after_fresh_evidence() {
        let mut led = EvidenceLedger::default();
        led.record_mutation("m1", "apply_patch", vec!["a.rs".into()]);
        led.record_verify("v1", "cargo\u{1f}test", 0);
        assert!(led.evidence_ref_is_fresh("v1"));
        led.record_step_receipt(CompleteStepReceipt {
            step_id: "edit".into(),
            step_text: "edit file".into(),
            summary: "done".into(),
            evidence_ref: "v1".into(),
        });
        assert_eq!(led.step_receipts.len(), 1);
        assert_eq!(led.step_receipts[0].evidence_ref, "v1");
    }

    /// R007b N2 accident shape: a reproduction that passes on the untouched
    /// tree proves nothing about a fix that has not been written.
    #[test]
    fn a_verification_that_passes_before_any_change_is_not_proof() {
        let mut led = EvidenceLedger::default();
        led.record_verify("c1", "vitest run repro", 0);
        assert_eq!(led.baseline_green_verifications().len(), 1);
        assert!(
            led.only_baseline_green_evidence(),
            "green on an unmodified tree must not read as proof"
        );
        assert!(
            !led.has_fresh_successful_verify(),
            "and it must not satisfy the fresh-verify gate either"
        );

        led.record_mutation("c2", "apply_patch", vec!["src/lib.rs".into()]);
        led.record_verify("c3", "vitest run repro", 0);
        assert!(!led.only_baseline_green_evidence());
        assert!(led.has_fresh_successful_verify());
        assert_eq!(led.baseline_green_verifications().len(), 1);
    }

    /// A FAILING check on an unmodified tree is exactly what a reproduction
    /// should look like, and must not be flagged.
    #[test]
    fn a_red_reproduction_on_the_baseline_is_not_flagged() {
        let mut led = EvidenceLedger::default();
        led.record_verify("c1", "vitest run repro", 1);
        assert!(led.baseline_green_verifications().is_empty());
        assert!(!led.only_baseline_green_evidence());
    }
}

#[cfg(test)]
mod finding_tests {
    use super::*;
    use crate::findings::{FindingError, FindingKind, FindingState};

    fn child_finding(led: &mut EvidenceLedger, blocking: bool) -> FindingRecord {
        let id = led.record_finding(
            FindingKind::Correctness,
            "boundary check missing",
            Some("src/auth.rs".into()),
            None,
            blocking,
        );
        led.finding(&id).expect("recorded").clone()
    }

    #[test]
    fn recording_assigns_stable_monotonic_ids_and_created_state() {
        let mut led = EvidenceLedger::default();
        let a = led.record_finding(FindingKind::Risk, "r1", None, None, false);
        let b = led.record_finding(FindingKind::Test, "r2", None, None, false);
        assert_ne!(a, b);
        assert_eq!(a, "f-1");
        assert_eq!(b, "f-2");
        let rec = led.finding(&a).expect("recorded finding is queryable");
        assert_eq!(rec.state, FindingState::Created);
        assert_eq!(rec.summary, "r1");
    }

    #[test]
    fn adoption_rekeys_and_lands_at_acknowledged() {
        let mut child = EvidenceLedger::default();
        let rec = child_finding(&mut child, true);
        let mut parent = EvidenceLedger::default();
        // Parent already owns a finding — child ids must not collide.
        parent.record_finding(FindingKind::Observation, "mine", None, None, false);
        let pid = parent.adopt_finding("agent-2", "reviewer", &rec);
        assert_eq!(pid, "f-2", "adoption uses the PARENT id sequence");
        let adopted = parent.finding(&pid).expect("adopted");
        assert_eq!(adopted.state, FindingState::Acknowledged);
        assert_eq!(adopted.source_child, "agent-2");
        assert_eq!(adopted.role, "reviewer");
        assert_eq!(adopted.summary, rec.summary);
        assert!(adopted.blocking, "blocking survives adoption");
    }

    #[test]
    fn resolution_walks_the_audited_lifecycle() {
        let mut led = EvidenceLedger::default();
        let rec = {
            let mut child = EvidenceLedger::default();
            child_finding(&mut child, true)
        };
        let id = led.adopt_finding("agent-1", "reviewer", &rec);
        assert_eq!(
            led.resolve_finding(&id, FindingState::Accepted, None, false),
            Ok(FindingState::Accepted)
        );
        assert_eq!(
            led.resolve_finding(&id, FindingState::Addressed, None, false),
            Ok(FindingState::Addressed)
        );
        // No fresh verification yet: it stays addressed and still blocks.
        assert_eq!(led.open_blocking_findings().len(), 1);
        assert_eq!(led.promote_addressed_findings(false), 0);
        assert_eq!(led.promote_addressed_findings(true), 1);
        assert_eq!(
            led.finding(&id).unwrap().state,
            FindingState::Verified,
            "fresh green verification promotes addressed findings"
        );
        assert!(led.open_blocking_findings().is_empty());
    }

    #[test]
    fn addressing_with_fresh_verification_promotes_immediately() {
        let mut led = EvidenceLedger::default();
        let rec = {
            let mut child = EvidenceLedger::default();
            child_finding(&mut child, true)
        };
        let id = led.adopt_finding("agent-1", "reviewer", &rec);
        led.resolve_finding(&id, FindingState::Accepted, None, false)
            .unwrap();
        assert_eq!(
            led.resolve_finding(&id, FindingState::Addressed, None, true),
            Ok(FindingState::Verified)
        );
    }

    #[test]
    fn rejection_requires_a_reason_and_is_durable() {
        let mut led = EvidenceLedger::default();
        let rec = {
            let mut child = EvidenceLedger::default();
            child_finding(&mut child, true)
        };
        let id = led.adopt_finding("agent-1", "reviewer", &rec);
        assert_eq!(
            led.resolve_finding(&id, FindingState::Rejected, None, false),
            Err(FindingError::RejectNeedsReason)
        );
        assert_eq!(
            led.resolve_finding(&id, FindingState::Rejected, Some("stale diff"), false),
            Ok(FindingState::Rejected)
        );
        let rec = led.finding(&id).unwrap();
        assert_eq!(rec.resolution_reason.as_deref(), Some("stale diff"));
        assert!(!rec.open_blocking(), "a rejected finding no longer blocks");
    }

    #[test]
    fn illegal_jumps_are_refused_with_the_states_named() {
        let mut led = EvidenceLedger::default();
        let rec = {
            let mut child = EvidenceLedger::default();
            child_finding(&mut child, false)
        };
        let id = led.adopt_finding("agent-1", "explorer", &rec);
        // Acknowledged -> Addressed skips judgment.
        assert_eq!(
            led.resolve_finding(&id, FindingState::Addressed, None, false),
            Err(FindingError::IllegalTransition {
                from: FindingState::Acknowledged,
                to: FindingState::Addressed,
            })
        );
        // The model can never write Verified directly.
        assert_eq!(
            led.resolve_finding(&id, FindingState::Verified, None, true),
            Err(FindingError::IllegalTransition {
                from: FindingState::Acknowledged,
                to: FindingState::Verified,
            })
        );
        assert_eq!(
            led.resolve_finding("f-99", FindingState::Accepted, None, false),
            Err(FindingError::UnknownId("f-99".into()))
        );
    }

    #[test]
    fn a_parent_finding_lands_acknowledged_and_can_block() {
        let mut led = EvidenceLedger::default();
        let id = led.record_parent_finding(
            "agent-1",
            "worker",
            FindingKind::Observation,
            "Worker Euclid did not complete scoped work",
            true,
        );
        let rec = led.finding(&id).expect("recorded");
        assert_eq!(rec.state, FindingState::Acknowledged);
        assert_eq!(rec.source_child, "agent-1");
        assert_eq!(rec.role, "worker");
        assert!(rec.blocking);
        assert_eq!(led.open_blocking_findings().len(), 1);
        led.resolve_finding(&id, FindingState::Rejected, Some("I'll finish it"), false)
            .unwrap();
        assert!(led.open_blocking_findings().is_empty());
    }

    #[test]
    fn open_blocking_ignores_non_blocking_and_settled_findings() {
        let mut led = EvidenceLedger::default();
        let blocking = {
            let mut child = EvidenceLedger::default();
            child_finding(&mut child, true)
        };
        let plain = {
            let mut child = EvidenceLedger::default();
            child_finding(&mut child, false)
        };
        let b = led.adopt_finding("agent-1", "reviewer", &blocking);
        led.adopt_finding("agent-1", "explorer", &plain);
        assert_eq!(led.open_blocking_findings().len(), 1);
        led.resolve_finding(&b, FindingState::Rejected, Some("not reachable"), false)
            .unwrap();
        assert!(led.open_blocking_findings().is_empty());
    }

    /// Session review debt carries into a fresh epoch; epoch evidence and
    /// settled findings do not, and the id sequence never restarts.
    #[test]
    fn carry_forward_keeps_open_findings_and_drops_epoch_evidence() {
        let mut led = EvidenceLedger::default();
        led.record_mutation("m1", "apply_patch", vec!["a.rs".into()]);
        led.record_verify("v1", "cargo\u{1f}test", 0);
        let open = {
            let mut child = EvidenceLedger::default();
            child_finding(&mut child, true)
        };
        let settled = {
            let mut child = EvidenceLedger::default();
            child_finding(&mut child, true)
        };
        let a = led.adopt_finding("reviewer-1", "reviewer", &open);
        let b = led.adopt_finding("reviewer-1", "reviewer", &settled);
        led.resolve_finding(&b, FindingState::Rejected, Some("dup"), false)
            .unwrap();

        let carried = led.carry_forward_findings().expect("open debt carries");
        assert_eq!(carried.findings.len(), 1);
        assert_eq!(carried.findings[0].id, a);
        assert!(
            carried.mutations.is_empty() && carried.verifications.is_empty(),
            "a new epoch must prove its own work"
        );
        assert_eq!(
            carried.next_finding_seq, led.next_finding_seq,
            "ids must never collide across epochs"
        );

        led.resolve_finding(&a, FindingState::Rejected, Some("also dup"), false)
            .unwrap();
        assert!(
            led.carry_forward_findings().is_none(),
            "nothing carries once every finding is settled"
        );
    }

    /// Carrying twice must not invent new ids or extra records. A fresh
    /// epoch seeds this snapshot; it must not look like a second adopt.
    #[test]
    fn carry_forward_is_idempotent() {
        let mut led = EvidenceLedger::default();
        let rec = {
            let mut child = EvidenceLedger::default();
            child_finding(&mut child, true)
        };
        let id = led.adopt_finding("reviewer-1", "reviewer", &rec);
        let first = led.carry_forward_findings().expect("open debt");
        let second = first.carry_forward_findings().expect("still open");
        assert_eq!(first.findings, second.findings);
        assert_eq!(first.next_finding_seq, second.next_finding_seq);
        assert_eq!(second.findings[0].id, id);
        assert_eq!(second.findings.len(), 1);
    }

    /// The replay contract: a pre-findings snapshot deserializes with empty
    /// findings, and a snapshot with findings restores states exactly.
    #[test]
    fn ledger_snapshots_replay_findings_state() {
        let mut led = EvidenceLedger::default();
        let rec = {
            let mut child = EvidenceLedger::default();
            child_finding(&mut child, true)
        };
        let id = led.adopt_finding("agent-1", "reviewer", &rec);
        led.resolve_finding(&id, FindingState::Accepted, None, false)
            .unwrap();
        let json = serde_json::to_string(&led).unwrap();
        let back: EvidenceLedger = serde_json::from_str(&json).unwrap();
        assert_eq!(back.finding(&id).unwrap().state, FindingState::Accepted);
        assert_eq!(back.next_finding_seq, led.next_finding_seq);

        let legacy: EvidenceLedger = serde_json::from_str(
            r#"{"plan":{"steps":[],"origin":"model"},"mutations":[],"verifications":[],"step_receipts":[],"intercepts":[],"next_seq":0}"#,
        )
        .unwrap();
        assert!(legacy.findings.is_empty());
    }
}

impl EvidenceLedger {
    /// Why completion truth forbids calling this run a success — `None` when
    /// nothing outstanding is known.
    ///
    /// THE canonical contract question, asked in exactly one place. Both the
    /// executor's completion gate and the engine's terminal boundary call it,
    /// so a run cannot reach success by arriving at the boundary through a
    /// door the gate never watched. That is not hypothetical: a forced
    /// closeout over a green workspace used to be mapped straight to verified
    /// while the contract still recorded an unwritten test.
    pub fn completion_debt(&self) -> Option<String> {
        if self.completion_contract_unavailable && self.completion_contract.is_none() {
            return Some("the completion contract could not be established".to_string());
        }
        let contract = self.completion_contract.as_ref()?;
        let open = contract.open_obligations(self);
        if open.is_empty() {
            return None;
        }
        Some(
            open.iter()
                .map(|(r, why)| {
                    let why = match why {
                        crate::OpenReason::NotAccountedFor => "not satisfied",
                        crate::OpenReason::MissingMechanicalEvidence => {
                            "no check demonstrates it over the current tree"
                        }
                        crate::OpenReason::MechanicalConstraintViolation => {
                            "the runtime's own record says this condition does not hold"
                        }
                        crate::OpenReason::MissingAuthoritativeProof => {
                            "satisfied by reading only — the runtime has no proof standard for it"
                        }
                        crate::OpenReason::Blocked => "blocked",
                    };
                    format!("{} ({}) — {}", r.id, r.text, why)
                })
                .collect::<Vec<_>>()
                .join("; "),
        )
    }
}

#[cfg(test)]
mod completion_debt_tests {
    use super::*;
    use crate::{
        CompletionContract, CompletionRequirement, RequirementKind, RequirementSource,
        RequirementStatus,
    };

    fn contract(status: RequirementStatus) -> CompletionContract {
        CompletionContract::new(vec![CompletionRequirement {
            id: "R1".into(),
            text: "the boundary rule is covered by a test".into(),
            kind: RequirementKind::Behavior,
            source: RequirementSource::OriginalGoal,
            status,
            evidence_policy: None,
            evidence: Vec::new(),
            acceptance_facets: Vec::new(),
        }])
    }

    /// A task with nothing outstanding owes nothing — a run that genuinely
    /// finished must still be able to succeed, however it reached its end.
    #[test]
    fn a_satisfied_contract_owes_nothing() {
        let ledger = EvidenceLedger {
            completion_contract: Some(contract(RequirementStatus::Satisfied)),
            ..Default::default()
        };
        assert_eq!(ledger.completion_debt(), None);
    }

    /// This is the scale-s800 false completion, at the boundary: obligations
    /// still open, and the route to the boundary is irrelevant.
    #[test]
    fn an_open_obligation_is_debt_whatever_the_route() {
        let ledger = EvidenceLedger {
            completion_contract: Some(contract(RequirementStatus::Pending)),
            ..Default::default()
        };
        let debt = ledger.completion_debt().expect("open obligation is debt");
        assert!(debt.contains("R1"), "the debt names what is owed: {debt}");
    }

    /// F6 at the terminal boundary: a violated file scope is debt at whatever
    /// door the run leaves by, forced closeout included. The boundary asks
    /// this one predicate, so a mechanically refuted obligation cannot be
    /// walked past by ending the run some other way.
    #[test]
    fn a_violated_scope_is_debt_at_the_terminal_boundary() {
        let mut ledger = EvidenceLedger {
            completion_contract: Some(CompletionContract::new(vec![CompletionRequirement {
                id: "R_SCOPE".into(),
                text: "no files outside internal/window may be modified".into(),
                kind: RequirementKind::Constraint,
                source: RequirementSource::OriginalGoal,
                // The judge already accounted for it as satisfied.
                status: RequirementStatus::Satisfied,
                evidence_policy: Some(crate::EvidencePolicy::MutationScope {
                    allowed_paths: vec!["internal/window".into()],
                }),
                evidence: Vec::new(),
                acceptance_facets: Vec::new(),
            }])),
            ..Default::default()
        };
        ledger.record_mutation(
            "c1",
            "apply_patch",
            vec!["internal/window/window.go".into()],
        );
        assert_eq!(
            ledger.completion_debt(),
            None,
            "in-scope work owes nothing at the boundary either"
        );
        ledger.record_mutation(
            "c2",
            "apply_patch",
            vec!["cmd/telemetryd/testdata/boundary_events.txt".into()],
        );
        let debt = ledger
            .completion_debt()
            .expect("a scope the record refutes is debt");
        assert!(debt.contains("R_SCOPE"), "{debt}");
        assert!(
            debt.contains("record says this condition does not hold"),
            "the debt says the record refutes it, not that proof is missing: {debt}"
        );
    }

    /// A chat turn has no obligations and owes nothing. Absence of a contract
    /// is only debt when a goal actually failed to build one.
    #[test]
    fn no_contract_and_none_expected_owes_nothing() {
        assert_eq!(EvidenceLedger::default().completion_debt(), None);
    }

    #[test]
    fn a_contract_that_could_not_be_built_is_debt() {
        let ledger = EvidenceLedger {
            completion_contract_unavailable: true,
            ..Default::default()
        };
        assert!(ledger.completion_debt().is_some());
    }
}

#[cfg(test)]
mod terminal_truth_matrix {
    use super::*;
    use crate::{
        CompletionContract, CompletionRequirement, EvidenceStrength, RequirementEvidence,
        RequirementKind, RequirementSource, RequirementStatus,
    };

    /// The debt a run carries is a property of the run, not of the door it
    /// leaves by. These cases are named for the terminal route that used to
    /// slip past — a forced closeout over a green workspace — but the
    /// predicate never sees the route, which is the entire point of the fix.
    fn ledger_with(reqs: Vec<CompletionRequirement>) -> EvidenceLedger {
        EvidenceLedger {
            completion_contract: Some(CompletionContract::new(reqs)),
            ..Default::default()
        }
    }

    fn req(kind: RequirementKind, status: RequirementStatus) -> CompletionRequirement {
        CompletionRequirement {
            id: "R1".into(),
            text: "the boundary rule is covered by a test".into(),
            kind,
            source: RequirementSource::OriginalGoal,
            status,
            evidence_policy: None,
            evidence: Vec::new(),
            acceptance_facets: Vec::new(),
        }
    }

    #[test]
    fn closeout_with_a_pending_requirement_owes_debt() {
        let l = ledger_with(vec![req(
            RequirementKind::Behavior,
            RequirementStatus::Pending,
        )]);
        assert!(l.completion_debt().is_some());
    }

    #[test]
    fn closeout_with_a_blocked_requirement_owes_debt() {
        let l = ledger_with(vec![req(
            RequirementKind::Behavior,
            RequirementStatus::Blocked,
        )]);
        assert!(l.completion_debt().is_some());
    }

    /// THE scale-s800 case. The workspace is green — the existing suite passes
    /// — and the judge called the test obligation satisfied on its own say-so.
    /// Green tests are evidence that the tree is healthy, not proof that the
    /// test the user asked for exists.
    #[test]
    fn a_green_workspace_does_not_settle_an_unproven_test_obligation() {
        let mut r = req(RequirementKind::Verification, RequirementStatus::Satisfied);
        r.evidence.push(RequirementEvidence {
            strength: EvidenceStrength::Semantic,
            detail: "I added a test for the boundary".into(),
            refs: Vec::new(),
        });
        let mut l = ledger_with(vec![r]);
        l.record_mutation(
            "c1",
            "apply_patch",
            vec!["internal/window/window.go".into()],
        );
        l.record_verify("v1", "go\u{1f}test", 0);
        let debt = l
            .completion_debt()
            .expect("a green suite is not the required test");
        assert!(debt.contains("no check demonstrates it"), "{debt}");
    }

    #[test]
    fn closeout_without_a_contract_that_should_exist_owes_debt() {
        let l = EvidenceLedger {
            completion_contract_unavailable: true,
            ..Default::default()
        };
        assert!(l.completion_debt().is_some());
    }

    /// Stale evidence is not evidence: a check that went green and was then
    /// invalidated by a later edit proves nothing about the tree that shipped.
    #[test]
    fn a_stale_check_owes_debt() {
        let mut r = req(RequirementKind::Verification, RequirementStatus::Satisfied);
        r.evidence.push(RequirementEvidence {
            strength: EvidenceStrength::Mechanical,
            detail: "go test ./...".into(),
            refs: Vec::new(),
        });
        let mut l = ledger_with(vec![r]);
        l.record_mutation("c1", "apply_patch", vec!["a.go".into()]);
        l.record_verify("v1", "go\u{1f}test", 0);
        l.record_mutation("c2", "apply_patch", vec!["b.go".into()]);
        assert!(l.completion_debt().is_some());
    }

    /// And the other direction, which matters just as much: a run that really
    /// did finish owes nothing, however it reached its end. Truth closure must
    /// not become blanket pessimism.
    #[test]
    fn a_genuinely_finished_run_owes_nothing_even_after_a_forced_closeout() {
        let mut r = req(RequirementKind::Verification, RequirementStatus::Satisfied);
        r.evidence.push(RequirementEvidence {
            strength: EvidenceStrength::Mechanical,
            detail: "go test ./... covering the boundary".into(),
            refs: Vec::new(),
        });
        r.evidence_policy = Some(crate::EvidencePolicy::CommandSuccess {
            commands: vec!["go\u{1f}test".into()],
            mode: crate::CommandMode::All,
        });
        let mut l = ledger_with(vec![r]);
        l.record_mutation(
            "c1",
            "apply_patch",
            vec!["internal/window/window_test.go".into()],
        );
        l.record_verify("v1", "go\u{1f}test", 0);
        assert_eq!(l.completion_debt(), None);
    }
}
