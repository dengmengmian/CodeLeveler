//! Process evidence ledger. Pure types — no I/O, no shell.
//!
//! Event log remains SoT for resume; this is the host in-memory projection
//! the mechanical readiness gate reads during a drive.

use serde::{Deserialize, Serialize};

use crate::findings::{FindingKind, FindingRecord};
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
pub struct InterceptRecord {
    pub kind: String,
    pub detail: String,
}

/// What this run mechanically did: mutations, verifications, intercepts and
/// multi-agent findings. Facts only — the ledger never says what those facts
/// prove about the user's intent, and a finding it holds is information the
/// model produced, never a gate.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct EvidenceLedger {
    pub plan: PlanState,
    pub mutations: Vec<MutationRecord>,
    pub verifications: Vec<VerifyRecord>,
    pub intercepts: Vec<InterceptRecord>,
    pub next_seq: u64,
    /// Every successful mutating tool call, INCLUDING repeat edits of files
    /// already in the modified set. `mutations` records only first-touch paths
    /// (its gating semantics are unchanged); this counter exists because R011
    /// showed that refinement — fixing files you already wrote — was invisible
    /// to every ledger, so the window judge starved it of progress credit.
    #[serde(default)]
    pub total_mutation_ops: u64,
    /// Sequence of the most recent mutating call, whether or not it produced
    /// a `MutationRecord`. `mutations` is keyed on first-touch paths, so a
    /// refinement edit of a file already in the set used to leave
    /// [`Self::last_mutation_seq`] where it was — and a test that passed
    /// before that edit kept reading as current. Freshness has to move on
    /// every change to the tree, not on every new path in it.
    #[serde(default)]
    pub last_mutation_op_seq: u64,
    /// Durable multi-agent findings (self-reported and adopted from children).
    /// Serde-default so pre-findings snapshots still replay.
    #[serde(default)]
    pub findings: Vec<FindingRecord>,
    /// Monotonic id source for findings owned by THIS ledger.
    #[serde(default)]
    pub next_finding_seq: u64,
}

impl EvidenceLedger {
    /// One successful mutating tool call happened (new paths or a re-edit).
    /// Advances the freshness sequence even when no path is new, so a
    /// verification recorded before this call is no longer current after it.
    pub fn note_mutation_op(&mut self) {
        self.total_mutation_ops = self.total_mutation_ops.saturating_add(1);
        self.next_seq = self.next_seq.saturating_add(1);
        self.last_mutation_op_seq = self.next_seq;
    }

    /// The sequence of the latest change to the tree: the later of the last
    /// first-touch record and the last mutating call of any kind.
    pub fn last_mutation_seq(&self) -> u64 {
        self.mutations
            .last()
            .map(|m| m.seq)
            .unwrap_or(0)
            .max(self.last_mutation_op_seq)
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

    pub fn has_fresh_successful_verify(&self) -> bool {
        let last_mut = self.last_mutation_seq();
        self.verifications
            .iter()
            .any(|v| v.exit_code == 0 && v.after_mutation_seq >= last_mut && last_mut > 0)
    }

    pub fn record_intercept(&mut self, kind: impl Into<String>, detail: impl Into<String>) {
        self.intercepts.push(InterceptRecord {
            kind: kind.into(),
            detail: detail.into(),
        });
    }

    /// Record a finding this agent itself established. Returns the id.
    pub fn record_finding(
        &mut self,
        kind: FindingKind,
        summary: impl Into<String>,
        file: Option<String>,
        symbol: Option<String>,
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
        });
        id
    }

    /// Adopt a child's finding into this (parent) ledger, re-keying the id so
    /// a child's ids never leak. Returns the parent-side id.
    pub fn adopt_finding(&mut self, source_child: &str, role: &str, rec: &FindingRecord) -> String {
        self.next_finding_seq = self.next_finding_seq.saturating_add(1);
        let id = format!("f-{}", self.next_finding_seq);
        self.findings.push(FindingRecord {
            id: id.clone(),
            source_child: source_child.to_string(),
            role: role.to_string(),
            ..rec.clone()
        });
        id
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
        led.record_verify("v2", "cargo\u{1f}test", 0);
        assert!(led.has_fresh_successful_verify());
    }
}

#[cfg(test)]
mod finding_tests {
    use super::*;
    use crate::findings::FindingKind;

    #[test]
    fn recording_assigns_stable_monotonic_ids() {
        let mut led = EvidenceLedger::default();
        let a = led.record_finding(FindingKind::Risk, "r1", None, None);
        let b = led.record_finding(FindingKind::Test, "r2", None, None);
        assert_eq!((a.as_str(), b.as_str()), ("f-1", "f-2"));
        assert_eq!(led.findings.len(), 2);
        assert_eq!(led.finding("f-1").unwrap().summary, "r1");
    }

    /// Adoption re-keys into the parent's own sequence: a child's `f-1` must
    /// never collide with the parent's.
    #[test]
    fn adoption_rekeys_and_attributes() {
        let mut child = EvidenceLedger::default();
        let cid = child.record_finding(
            FindingKind::Correctness,
            "boundary check missing",
            Some("src/auth.rs".into()),
            None,
        );
        let rec = child.finding(&cid).unwrap().clone();

        let mut parent = EvidenceLedger::default();
        parent.record_finding(FindingKind::Risk, "parent's own", None, None);
        let adopted = parent.adopt_finding("agent-1", "explorer", &rec);

        assert_eq!(adopted, "f-2", "the parent's sequence, not the child's");
        let f = parent.finding(&adopted).unwrap();
        assert_eq!(f.source_child, "agent-1");
        assert_eq!(f.role, "explorer");
        assert_eq!(f.summary, "boundary check missing");
        assert_eq!(f.file.as_deref(), Some("src/auth.rs"));
    }

    /// A finding is information. Nothing in the ledger can turn one into a
    /// reason to refuse a completion, which is what the deleted `blocking`
    /// flag and its state machine did.
    #[test]
    fn the_ledger_exposes_no_gate_over_findings() {
        let mut led = EvidenceLedger::default();
        led.record_finding(FindingKind::Correctness, "looks wrong", None, None);
        led.record_mutation("c1", "apply_patch", vec!["src/lib.rs".into()]);
        led.record_verify("v1", "cargo\u{1f}test", 0);
        assert!(
            led.has_fresh_successful_verify(),
            "an unjudged finding does not make verification stale"
        );
    }
}
