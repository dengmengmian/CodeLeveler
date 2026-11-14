//! The completion-verification record.
//!
//! A completion is a *declaration*: the model states that the recorded
//! requirements are done. The evidence is the runtime's own execution record.
//! This module is the boundary between the two: [`VerificationRecord`] names
//! the requirement, the execution the runtime observed, and the exit and
//! workspace facts that make the association checkable. [`VerificationStatus`]
//! is the aggregate verdict persisted with the terminal.
//!
//! The record never says the user's objective was met. It says what the
//! runtime could check and what it could not.

use serde::{Deserialize, Serialize};

use crate::runtime::VerificationStatus;

/// The verification verdict carried by an accepted completion.
///
/// Absent from the terminal entirely when the task declared no verification
/// obligation: there was nothing to assess, which is neither verified nor
/// unverified.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VerificationReport {
    /// The aggregate the runtime reached.
    pub status: VerificationStatus,
    /// One record per recorded requirement the gate assessed, in declaration
    /// order. Empty when the task had no recorded requirement set and the
    /// verdict comes from the goal-level verification declaration alone.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub records: Vec<VerificationRecord>,
    /// Goal-wide diagnostics when no individual requirement owns the check.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub notes: Vec<String>,
}

/// What the runtime could establish about one recorded requirement.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum VerificationRecordState {
    /// A mechanical check matched the execution record. For a `tests`
    /// requirement, that record is the exact declared command's passing run:
    /// the check proves the command contract, not that any test executed.
    Verified,
    /// The requirement's completion contract is model judgement: the model
    /// delivered it, and no mechanical check applies or was declared. This is
    /// a normal completion, not an evidence gap; the record keeps the
    /// difference from [`Self::Verified`] for diagnostics and audit.
    Reported,
    /// The runtime did not establish the fact and the requirement needed it:
    /// the observation boundary is unknown, the evidence is missing, or the
    /// model accounted the requirement as unverifiable.
    Unverified,
}

/// One requirement's place in the accepted completion.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VerificationRecord {
    /// The stable requirement id the account was keyed by (`R1`, `R2`, …).
    pub requirement_id: String,
    /// User-facing requirement text, absent on older reports.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub requirement_text: Option<String>,
    /// The declared requirement kind, as its wire label (`artifact`, `tests`,
    /// `named`, …). The terminal stores the label rather than the enum so a
    /// reader never has to import the Coding requirement vocabulary.
    pub kind: String,
    pub state: VerificationRecordState,
    /// What the runtime established (verified) or could not (unverified), in
    /// the same words the completion receipt uses.
    pub detail: String,
    /// The execution the check resolved to, when there was one. `call_id`
    /// names the exact tool call; `command` is the command as recorded.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub call_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub command: Option<String>,
    /// The command's exit status, when the tool reported one. A killed or
    /// timed-out command may have none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i32>,
    /// The workspace version the run observed: the monotonically increasing
    /// mutation count at the moment the command finished. A verification run
    /// must have a version at least the completion's, so "after the last
    /// change" is a comparison, not a timestamp guess.
    #[serde(default)]
    pub workspace_version: u64,
    /// The model step that ran the cited command; ordering only.
    #[serde(default)]
    pub model_step: u32,
}

impl VerificationReport {
    /// Build a report from its parts.
    pub fn new(status: VerificationStatus, records: Vec<VerificationRecord>) -> Self {
        Self {
            status,
            records,
            notes: Vec::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_report_round_trips_without_losing_the_evidence_fields() {
        let report = VerificationReport::new(
            VerificationStatus::Verified,
            vec![VerificationRecord {
                requirement_id: "R1".to_string(),
                requirement_text: None,
                kind: "tests".to_string(),
                state: VerificationRecordState::Verified,
                detail: "`cargo test` passed (step 3)".to_string(),
                call_id: Some("call-1".to_string()),
                command: Some("cargo test".to_string()),
                exit_code: Some(0),
                workspace_version: 2,
                model_step: 3,
            }],
        );
        let encoded = serde_json::to_string(&report).expect("serialize");
        let decoded: VerificationReport = serde_json::from_str(&encoded).expect("deserialize");
        assert_eq!(decoded, report);
    }

    #[test]
    fn a_legacy_report_without_records_decodes() {
        let decoded: VerificationReport =
            serde_json::from_str(r#"{"status":"unverified"}"#).expect("decode");
        assert_eq!(decoded.status, VerificationStatus::Unverified);
        assert!(decoded.records.is_empty());
    }
}
