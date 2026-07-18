//! Process evidence ledger (Delivery). Pure types — no I/O, no shell.
//!
//! Event log remains SoT for resume; this is the host in-memory projection
//! the readiness gate reads during a drive.

use serde::{Deserialize, Serialize};

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
}

impl EvidenceLedger {
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
    pub fn has_fresh_successful_verify(&self) -> bool {
        let last_mut = self.last_mutation_seq();
        self.verifications
            .iter()
            .any(|v| v.exit_code == 0 && v.after_mutation_seq >= last_mut && last_mut > 0)
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
}
