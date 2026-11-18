//! Process evidence ledger. Pure types — no I/O, no shell.
//!
//! Event log remains SoT for resume; this is the host in-memory projection
//! the runtime reads during a drive.

use serde::{Deserialize, Serialize};

use crate::findings::{FindingKind, FindingRecord, ReviewVerdict};
use crate::plan::PlanState;

/// How completely one mutation op's changed paths are known.
///
/// The workspace version exists to answer "did the tree move since this check
/// passed?". A record whose paths are unknown cannot answer "no": it must
/// invalidate exactly like a known change, otherwise an unobserved write would
/// silently pass a stale check.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MutationScope {
    /// The execution reported the complete set of paths it changed.
    #[default]
    Known,
    /// The execution ran with write authority but its changed paths could not
    /// be observed. The version advances; the path set is honestly unknown.
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MutationRecord {
    pub seq: u64,
    pub tool_call_id: String,
    pub tool: String,
    pub paths: Vec<String>,
    /// Whether `paths` is the complete changed set. Older records predate the
    /// field and are known-path records.
    #[serde(default)]
    pub scope: MutationScope,
    /// The workspace version this op advanced the tree to. It is the link
    /// between "a mutation happened" and "which version observed it": the
    /// sequence number is independent of `total_mutation_ops`, so it cannot
    /// answer that question on its own. Older records predate the field (0).
    #[serde(default)]
    pub mutation_ops: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InterceptRecord {
    pub kind: String,
    pub detail: String,
}

/// What this run mechanically did: mutations, intercepts and
/// multi-agent findings. Facts only — the ledger never says what those facts
/// prove about the user's intent, and a finding it holds is information the
/// model produced, never a gate.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct EvidenceLedger {
    pub plan: PlanState,
    pub mutations: Vec<MutationRecord>,
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
    /// [`Self::last_mutation_seq`] where it was. Freshness has to move on every
    /// change to the tree, not on every new path in it.
    #[serde(default)]
    pub last_mutation_op_seq: u64,
    /// Durable multi-agent findings (self-reported and adopted from children).
    /// Serde-default so pre-findings snapshots still replay.
    #[serde(default)]
    pub findings: Vec<FindingRecord>,
    /// Monotonic id source for findings owned by THIS ledger.
    #[serde(default)]
    pub next_finding_seq: u64,
    /// The reviewer's structured coverage, limitations and conclusion.
    ///
    /// Written only by the `report_review` tool. `None` means the reviewer did
    /// not declare one — it is never inferred from findings or final prose.
    /// Serde-default so every pre-review ledger snapshot still replays.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub review: Option<ReviewVerdict>,
    /// The independent investigator's structured facts, scope, questions and
    /// limitations. Written only by `report_investigation` on an investigator
    /// seat; the parent reviewer reads it as evidence, never as a verdict.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub investigation: Option<crate::InvestigationRecord>,
    /// How many structured evidence changes this ledger has recorded: each
    /// distinct child settlement, plus the Lead's own fact changes (a finding
    /// recorded, or an adjudication that changed a decision). A report is
    /// bound to this number, so any later change makes the report detectably
    /// stale — not only a late child settlement.
    #[serde(default)]
    pub evidence_revision: u64,
    /// Ids of the children whose settlements this ledger already folded. Makes
    /// the revision idempotent under duplicate delivery or replay.
    #[serde(default)]
    pub settled_children: Vec<String>,
    /// How many formal reports the reviewer has submitted. A later
    /// `report_review` replaces the earlier draft and increments this.
    #[serde(default)]
    pub report_revision: u32,
}

impl EvidenceLedger {
    /// One successful mutating tool call happened (new paths or a re-edit).
    /// Advances the freshness sequence even when no path is new.
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
            scope: MutationScope::Known,
            mutation_ops: self.total_mutation_ops,
        });
    }

    /// Record one mutating execution whose changed paths could not be
    /// observed. The tree may have moved; the version must advance so no
    /// earlier verification can claim the current tree.
    ///
    /// Returns whether a new record was written. The same execution identity
    /// is recorded once: a repeated completion attempt must not inflate the
    /// version it is judging against.
    pub fn record_unknown_mutation(
        &mut self,
        tool_call_id: impl Into<String>,
        tool: impl Into<String>,
    ) -> bool {
        let tool_call_id = tool_call_id.into();
        let tool = tool.into();
        if self
            .mutations
            .iter()
            .any(|record| record.tool == tool && record.tool_call_id == tool_call_id)
        {
            return false;
        }
        self.note_mutation_op();
        self.next_seq = self.next_seq.saturating_add(1);
        self.mutations.push(MutationRecord {
            seq: self.next_seq,
            tool_call_id,
            tool,
            paths: Vec::new(),
            scope: MutationScope::Unknown,
            mutation_ops: self.total_mutation_ops,
        });
        true
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
        self.record_finding_with(
            kind,
            summary,
            file,
            symbol,
            crate::findings::FindingDetails::default(),
        )
    }

    /// [`Self::record_finding`] with the optional review fields. Ordinary
    /// explorer/worker calls pass [`crate::FindingDetails::default`], which
    /// serializes to exactly the old shape.
    pub fn record_finding_with(
        &mut self,
        kind: FindingKind,
        summary: impl Into<String>,
        file: Option<String>,
        symbol: Option<String>,
        details: crate::findings::FindingDetails,
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
            details,
        });
        self.note_evidence_change();
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

    /// Adopt an INVESTIGATOR's finding as a candidate, not a formal defect.
    ///
    /// The record keeps the investigator's identity and the original child-side
    /// id; only the Lead Reviewer's structured adjudication can turn it into a
    /// formal finding. This is the one entry point used when a review folds a
    /// settlement, so a child result can never silently become a defect count.
    pub fn adopt_candidate(
        &mut self,
        source_child: &str,
        role: &str,
        rec: &FindingRecord,
    ) -> String {
        self.next_finding_seq = self.next_finding_seq.saturating_add(1);
        let id = format!("f-{}", self.next_finding_seq);
        let mut details = rec.details.clone();
        details.disposition = Some(crate::findings::FindingDisposition::Candidate);
        details.source_finding_id = Some(rec.id.clone());
        self.findings.push(FindingRecord {
            id: id.clone(),
            source_child: source_child.to_string(),
            role: role.to_string(),
            details,
            ..rec.clone()
        });
        id
    }

    /// One structured fact changed. Monotonic and cheap; the report binding
    /// compares it, it is not a count of children. A child settlement already
    /// covers the candidates it folds, so adoption does not bump again.
    fn note_evidence_change(&mut self) {
        self.evidence_revision = self.evidence_revision.saturating_add(1);
    }

    /// Record that this ledger received one child's terminal result.
    ///
    /// Idempotent by child id: a retried or replayed settlement never advances
    /// the evidence revision twice. Returns whether the revision moved.
    pub fn note_child_settlement(&mut self, child_id: &str) -> bool {
        if self.settled_children.iter().any(|id| id == child_id) {
            return false;
        }
        self.settled_children.push(child_id.to_string());
        self.evidence_revision = self.evidence_revision.saturating_add(1);
        self.next_seq = self.next_seq.saturating_add(1);
        true
    }

    /// Apply the Lead Reviewer's decision to one finding.
    ///
    /// Validation lives here, next to the record it changes: every value comes
    /// from the typed tool boundary, never from the model's prose.
    pub fn adjudicate_finding(
        &mut self,
        id: &str,
        decision: crate::findings::FindingAdjudication,
    ) -> Result<(), String> {
        use crate::findings::FindingDisposition;

        if !self.findings.iter().any(|finding| finding.id == id) {
            return Err(format!("unknown finding id `{id}`"));
        }
        if decision.disposition == FindingDisposition::Duplicate {
            let Some(target) = decision.duplicate_of.as_deref() else {
                return Err(
                    "disposition `duplicate` requires `duplicate_of` naming the surviving finding."
                        .to_string(),
                );
            };
            if target == id {
                return Err("a finding cannot be a duplicate of itself.".to_string());
            }
            let Some(survivor) = self.findings.iter().find(|finding| finding.id == target) else {
                return Err(format!(
                    "duplicate_of `{target}` is not a finding in this review."
                ));
            };
            if matches!(
                survivor.details.disposition,
                Some(FindingDisposition::Duplicate | FindingDisposition::Rejected)
            ) {
                return Err(format!(
                    "duplicate_of `{target}` must name a surviving finding, not a \
                     rejected or merged one."
                ));
            }
        }
        let Some(finding) = self.findings.iter_mut().find(|finding| finding.id == id) else {
            return Err(format!("unknown finding id `{id}`"));
        };
        let before = finding.details.clone();
        finding.details.disposition = Some(decision.disposition);
        finding.details.disposition_reason = decision.reason;
        finding.details.disposition_evidence = decision.evidence;
        finding.details.duplicate_of = if decision.disposition == FindingDisposition::Duplicate {
            decision.duplicate_of
        } else {
            None
        };
        if decision.disposition == FindingDisposition::Adopted {
            if let Some(severity) = decision.severity {
                finding.details.severity = Some(severity);
            }
            if let Some(confidence) = decision.confidence {
                finding.details.confidence = Some(confidence);
            }
            if let Some(title) = decision.title {
                finding.details.title = Some(title);
            }
        }
        // Repeating the identical decision is not a new fact: it must not grow
        // the report's evidence revision.
        if finding.details != before {
            self.next_seq = self.next_seq.saturating_add(1);
            self.note_evidence_change();
        }
        Ok(())
    }

    pub fn finding(&self, id: &str) -> Option<&FindingRecord> {
        self.findings.iter().find(|f| f.id == id)
    }

    /// The ids of the review's formal findings, in allocation order. A formal
    /// finding is the Lead's own record or an explicitly adopted candidate;
    /// candidates, rejections and duplicates keep their ids as evidence but
    /// are not part of the formal set. Ids are never renumbered: a gap left by
    /// a duplicate or rejection stays a gap.
    pub fn formal_finding_ids(&self) -> Vec<&str> {
        self.findings
            .iter()
            .filter(|finding| finding.is_formal())
            .map(|finding| finding.id.as_str())
            .collect()
    }

    /// Validate one `report_review` submission against this ledger's durable
    /// finding identities.
    ///
    /// `declared` is the structured formal set the report claims to present;
    /// `report` is the Markdown body. Identity belongs to the ledger: every id
    /// in either place must name a formal record of THIS review. Candidate,
    /// rejected and duplicate records keep their ids for traceability, but a
    /// report may not present them as formal findings.
    pub fn validate_review_report(&self, declared: &[String], report: &str) -> Result<(), String> {
        let formal = self.formal_finding_ids();
        let formal_text = || formal_ids_text(&formal);

        // The declared set is a conformance check, not a second identity
        // source: it must be exactly the formal set this ledger owns.
        let mut seen: Vec<&str> = Vec::with_capacity(declared.len());
        for id in declared {
            if seen.contains(&id.as_str()) {
                return Err(format!(
                    "report_review `formal_findings` lists `{id}` more than once. {}",
                    formal_text()
                ));
            }
            match self.finding(id) {
                None => {
                    return Err(format!(
                        "report_review `formal_findings` names `{id}`, which is not a \
                         finding in this review. {}",
                        formal_text()
                    ));
                }
                Some(record) if !record.is_formal() => {
                    return Err(format!(
                        "report_review `formal_findings` names `{id}`, which is {}, not a \
                         formal finding. {}",
                        disposition_text(record),
                        formal_text()
                    ));
                }
                Some(_) => {}
            }
            seen.push(id);
        }
        for id in &formal {
            if !seen.contains(id) {
                return Err(format!(
                    "report_review `formal_findings` omits `{id}`, a formal finding of \
                     this review. {}",
                    formal_text()
                ));
            }
        }

        // The body may cite formal ids only. Referential integrity is checked
        // against the ledger, outside quoted code; this never rewrites the
        // reviewer's prose.
        for reference in report_id_references(report) {
            match self.finding(reference) {
                None => {
                    return Err(format!(
                        "the report body cites `{reference}`, which is not a finding in \
                         this review. {}",
                        formal_text()
                    ));
                }
                Some(record) if !record.is_formal() => {
                    return Err(format!(
                        "the report body cites `{reference}`, which is {}, not a formal \
                         finding. Candidate, rejected and duplicate records are rendered \
                         from structured data; do not cite their ids as formal findings. {}",
                        disposition_text(record),
                        formal_text()
                    ));
                }
                Some(_) => {}
            }
        }
        Ok(())
    }

    /// Store the reviewer's structured declaration. A later call replaces the
    /// earlier one: the ledger holds the reviewer's current statement, not a
    /// history of drafts.
    ///
    /// The runtime stamps both revision fields here. The model supplies its
    /// conclusion, coverage, limitations and report body; it cannot choose
    /// which evidence set its report claims to cover.
    pub fn set_review_verdict(&mut self, mut verdict: ReviewVerdict) {
        self.next_seq = self.next_seq.saturating_add(1);
        self.report_revision = self.report_revision.saturating_add(1);
        verdict.report_revision = Some(self.report_revision);
        verdict.evidence_revision = Some(self.evidence_revision);
        self.review = Some(verdict);
    }

    /// Store the investigator's structured result. Like the review verdict, a
    /// later call replaces the earlier one rather than appending a second
    /// truth for the same child.
    pub fn set_investigation(&mut self, record: crate::InvestigationRecord) {
        self.next_seq = self.next_seq.saturating_add(1);
        self.investigation = Some(record);
    }
}

/// How a non-formal record is named in a report-identity error.
fn disposition_text(record: &FindingRecord) -> String {
    use crate::findings::FindingDisposition;
    match record.details.disposition {
        Some(FindingDisposition::Candidate) => "an unresolved candidate".to_string(),
        Some(FindingDisposition::Rejected) => "a rejected candidate".to_string(),
        Some(FindingDisposition::Duplicate) => match record.details.duplicate_of.as_deref() {
            Some(target) => format!("a duplicate of `{target}`"),
            None => "a duplicate".to_string(),
        },
        // An adopted record is formal; an absent disposition is the seat's own
        // record, also formal. Neither can reach this branch.
        _ => "not a formal finding".to_string(),
    }
}

/// The formal ids an error message can offer as the correct targets.
fn formal_ids_text(formal: &[&str]) -> String {
    const MAX_LISTED: usize = 32;
    if formal.is_empty() {
        return "This review has no formal findings.".to_string();
    }
    let mut ids = formal
        .iter()
        .take(MAX_LISTED)
        .copied()
        .collect::<Vec<_>>()
        .join(", ");
    if formal.len() > MAX_LISTED {
        ids.push_str(&format!(" (+{} more)", formal.len() - MAX_LISTED));
    }
    format!("Formal finding ids in this review: {ids}.")
}

/// The `f-<digits>` identifiers a reader could take as finding references.
///
/// Fenced code blocks and inline code spans are quoted material, not report
/// references; ids inside them are ignored. The grammar is the ledger's own id
/// shape, so this is a referential-integrity check, not prose interpretation.
fn report_id_references(body: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let mut fence: Option<(char, usize)> = None;
    for line in body.lines() {
        let trimmed = line.trim_start();
        if let Some((marker, length)) = fence {
            if closes_fence(trimmed, marker, length) {
                fence = None;
            }
            continue;
        }
        if let Some(open) = opens_fence(trimmed) {
            fence = Some(open);
            continue;
        }
        scan_line_outside_code(line, &mut out);
    }
    out
}

fn opens_fence(line: &str) -> Option<(char, usize)> {
    for marker in ['`', '~'] {
        let length = line.chars().take_while(|ch| *ch == marker).count();
        if length >= 3 {
            return Some((marker, length));
        }
    }
    None
}

fn closes_fence(line: &str, marker: char, opened_with: usize) -> bool {
    let length = line.chars().take_while(|ch| *ch == marker).count();
    length >= opened_with && line[length..].trim().is_empty()
}

fn scan_line_outside_code<'a>(line: &'a str, out: &mut Vec<&'a str>) {
    let bytes = line.as_bytes();
    let mut index = 0;
    let mut inline_code: Option<usize> = None;
    while index < bytes.len() {
        if bytes[index] == b'`' {
            let run = bytes[index..].iter().take_while(|b| **b == b'`').count();
            inline_code = match inline_code {
                None => Some(run),
                Some(opened_with) if run == opened_with => None,
                Some(opened_with) => Some(opened_with),
            };
            index += run;
            continue;
        }
        if inline_code.is_none()
            && bytes[index] == b'f'
            && bytes.get(index + 1) == Some(&b'-')
            && bytes.get(index + 2).is_some_and(u8::is_ascii_digit)
        {
            let start = index;
            let start_ok = start == 0 || !is_id_byte(bytes[start - 1]);
            let mut end = index + 2;
            while end < bytes.len() && bytes[end].is_ascii_digit() {
                end += 1;
            }
            let end_ok = end == bytes.len() || !is_id_byte(bytes[end]);
            if start_ok && end_ok {
                out.push(&line[start..end]);
            }
            index = end;
            continue;
        }
        index += 1;
    }
}

fn is_id_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-'
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

    /// The verdict is a model declaration carried by the ledger. A snapshot
    /// written before manual review has no `review` key and must replay.
    #[test]
    fn a_review_verdict_is_stored_verbatim_and_absent_by_default() {
        let mut led = EvidenceLedger::default();
        assert!(led.review.is_none());
        let verdict = crate::findings::ReviewVerdict {
            conclusion: crate::findings::ReviewConclusion::Inconclusive,
            coverage: Vec::new(),
            limitations: vec![crate::findings::ReviewLimitation {
                kind: crate::findings::ReviewLimitationKind::Environment,
                detail: "no production database".into(),
            }],
            report: Some("the formal report".into()),
            report_revision: None,
            evidence_revision: None,
        };
        led.set_review_verdict(verdict.clone());
        let stored = led.review.as_ref().expect("stored verdict");
        assert_eq!(stored.conclusion, verdict.conclusion);
        assert_eq!(stored.report, verdict.report);
        assert_eq!(stored.report_revision, Some(1), "runtime stamps revision 1");
        assert_eq!(stored.evidence_revision, Some(0), "no child settled yet");
        // A second submission is a REVISION of the same statement, not a second
        // independent truth.
        led.set_review_verdict(verdict);
        assert_eq!(led.review.as_ref().and_then(|v| v.report_revision), Some(2));
        let json = serde_json::to_string(&led).unwrap();
        assert_eq!(serde_json::from_str::<EvidenceLedger>(&json).unwrap(), led);
    }

    /// The investigation record rides the same ledger and the same snapshot
    /// mechanism as the review verdict. An older snapshot has no key.
    #[test]
    fn an_investigation_is_stored_on_the_same_ledger_and_absent_by_default() {
        let mut led = EvidenceLedger::default();
        assert!(led.investigation.is_none());
        let record = crate::InvestigationRecord {
            objective: "核对订单状态写入路径".into(),
            observed_facts: vec![crate::InvestigationFact {
                statement: "结算模块会写入订单状态".into(),
                evidence_refs: vec!["src/settle.rs:42".into()],
            }],
            business_constraints: vec!["已终止订单不参与正常状态推进".into()],
            examined_scope: vec!["src/order.rs".into()],
            unresolved_questions: vec!["两条写入路径组合后是否覆盖终止事实？".into()],
            limitations: vec!["只读静态检查".into()],
            evidence_refs: vec!["src/settle.rs:42".into()],
            review_revision: Some("0123456789abcdef".into()),
        };
        led.set_investigation(record.clone());
        assert_eq!(led.investigation.as_ref(), Some(&record));
        let json = serde_json::to_string(&led).unwrap();
        assert_eq!(serde_json::from_str::<EvidenceLedger>(&json).unwrap(), led);
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
    /// reason to refuse a completion.
    #[test]
    fn the_ledger_exposes_no_gate_over_findings() {
        let mut led = EvidenceLedger::default();
        led.record_finding(FindingKind::Correctness, "looks wrong", None, None);
        assert_eq!(led.findings.len(), 1);
    }

    /// "Could not observe the change" is not "did not change". The version
    /// must advance exactly like a known mutation, once per execution identity,
    /// and the record must say its paths are unknown rather than empty-because-
    /// nothing-new.
    #[test]
    fn an_unobservable_mutation_advances_the_version_once() {
        let mut led = EvidenceLedger::default();
        assert!(led.record_unknown_mutation("call-1", "shell_command"));
        assert_eq!(led.total_mutation_ops, 1);
        assert_eq!(led.mutations.len(), 1);
        assert_eq!(led.mutations[0].scope, MutationScope::Unknown);
        assert!(led.mutations[0].paths.is_empty());
        assert_eq!(led.last_mutation_seq(), led.mutations[0].seq);

        // A repeated completion attempt judges the same execution; it must not
        // inflate the version against itself.
        assert!(!led.record_unknown_mutation("call-1", "shell_command"));
        assert_eq!(led.total_mutation_ops, 1);
        assert_eq!(led.mutations.len(), 1);

        // The same id from a different tool is a different execution.
        assert!(led.record_unknown_mutation("call-1", "run_command"));
        assert_eq!(led.total_mutation_ops, 2);
    }

    /// A known-path record stays `Known`, and pre-field snapshots replay as
    /// known records.
    #[test]
    fn mutation_scope_survives_replay_and_defaults_to_known() {
        let mut led = EvidenceLedger::default();
        led.record_mutation("call-1", "apply_patch", vec!["src/lib.rs".into()]);
        assert_eq!(led.mutations[0].scope, MutationScope::Known);

        let legacy: EvidenceLedger = serde_json::from_str(
            r#"{"plan":{"steps":[]},"mutations":[{"seq":1,"tool_call_id":"c","tool":"t","paths":["a"]}],
                "intercepts":[],"next_seq":1}"#,
        )
        .expect("a snapshot without the scope field still replays");
        assert_eq!(legacy.mutations[0].scope, MutationScope::Known);
    }

    fn child_finding() -> FindingRecord {
        let mut child = EvidenceLedger::default();
        let id = child.record_finding(
            FindingKind::Correctness,
            "load() has no owner filter",
            Some("src/billing.rs".into()),
            None,
        );
        child.finding(&id).unwrap().clone()
    }

    /// A settlement produces a CANDIDATE. It is not a formal defect until the
    /// Lead Reviewer explicitly adopts it.
    #[test]
    fn adoption_from_an_investigator_starts_as_a_candidate() {
        let mut parent = EvidenceLedger::default();
        let id = parent.adopt_candidate("agent-1", "investigator", &child_finding());
        let finding = parent.finding(&id).unwrap();
        assert!(finding.is_candidate());
        assert!(!finding.is_formal());
        assert_eq!(finding.source_child, "agent-1");
        assert_eq!(finding.role, "investigator");
        assert_eq!(
            finding.details.source_finding_id.as_deref(),
            Some("f-1"),
            "the investigator's own id survives adoption"
        );
    }

    #[test]
    fn the_lead_can_adopt_reject_leave_unresolved_and_merge() {
        use crate::findings::{FindingAdjudication, FindingDisposition};

        let mut led = EvidenceLedger::default();
        let adopted = led.adopt_candidate("a1", "investigator", &child_finding());
        let rejected = led.adopt_candidate("a2", "investigator", &child_finding());
        let merged = led.adopt_candidate("a3", "investigator", &child_finding());

        led.adjudicate_finding(
            &adopted,
            FindingAdjudication {
                disposition: FindingDisposition::Adopted,
                reason: Some("confirmed on the order path".into()),
                evidence: vec!["src/order.rs:12".into()],
                severity: Some(crate::findings::FindingSeverity::P1),
                ..FindingAdjudication::default()
            },
        )
        .unwrap();
        led.adjudicate_finding(
            &rejected,
            FindingAdjudication {
                disposition: FindingDisposition::Rejected,
                reason: Some("the caller checks the owner".into()),
                ..FindingAdjudication::default()
            },
        )
        .unwrap();
        led.adjudicate_finding(
            &merged,
            FindingAdjudication {
                disposition: FindingDisposition::Duplicate,
                duplicate_of: Some(adopted.clone()),
                ..FindingAdjudication::default()
            },
        )
        .unwrap();

        let adopted_rec = led.finding(&adopted).unwrap();
        assert!(adopted_rec.is_formal());
        assert_eq!(
            adopted_rec.details.severity,
            Some(crate::findings::FindingSeverity::P1),
            "an adoption may adjust severity"
        );
        assert_eq!(
            adopted_rec.details.disposition_reason.as_deref(),
            Some("confirmed on the order path")
        );
        let rejected_rec = led.finding(&rejected).unwrap();
        assert!(!rejected_rec.is_formal());
        assert_eq!(
            rejected_rec.details.disposition,
            Some(FindingDisposition::Rejected)
        );
        let merged_rec = led.finding(&merged).unwrap();
        assert_eq!(
            merged_rec.details.duplicate_of.as_deref(),
            Some(adopted.as_str())
        );
        assert!(!merged_rec.is_formal());

        // Leaving a candidate unmentioned keeps it unresolved.
        let unresolved = led.adopt_candidate("a4", "investigator", &child_finding());
        assert!(led.finding(&unresolved).unwrap().is_candidate());

        // An explicit `unresolved` clears a previous decision.
        led.adjudicate_finding(
            &adopted,
            FindingAdjudication {
                disposition: FindingDisposition::Candidate,
                ..FindingAdjudication::default()
            },
        )
        .unwrap();
        assert!(led.finding(&adopted).unwrap().is_candidate());
    }

    #[test]
    fn adjudication_refuses_unknown_targets_and_invalid_duplicates() {
        use crate::findings::{FindingAdjudication, FindingDisposition};

        let mut led = EvidenceLedger::default();
        assert!(
            led.adjudicate_finding(
                "f-9",
                FindingAdjudication {
                    disposition: FindingDisposition::Adopted,
                    ..FindingAdjudication::default()
                }
            )
            .is_err()
        );

        let id = led.adopt_candidate("a1", "investigator", &child_finding());
        assert!(
            led.adjudicate_finding(
                &id,
                FindingAdjudication {
                    disposition: FindingDisposition::Duplicate,
                    duplicate_of: None,
                    ..FindingAdjudication::default()
                }
            )
            .is_err()
        );
        assert!(
            led.adjudicate_finding(
                &id,
                FindingAdjudication {
                    disposition: FindingDisposition::Duplicate,
                    duplicate_of: Some(id.clone()),
                    ..FindingAdjudication::default()
                }
            )
            .is_err()
        );
    }

    /// A duplicate delivery of the same settlement does not advance the
    /// evidence revision twice.
    #[test]
    fn the_evidence_revision_is_idempotent_per_child() {
        let mut led = EvidenceLedger::default();
        assert!(led.note_child_settlement("agent-1"));
        assert!(!led.note_child_settlement("agent-1"));
        assert!(led.note_child_settlement("agent-2"));
        assert_eq!(led.evidence_revision, 2);
        assert_eq!(led.settled_children, vec!["agent-1", "agent-2"]);
    }

    /// The report's stamped evidence revision is the ledger's revision at
    /// submission time, so a later settlement makes the report detectably
    /// stale.
    #[test]
    fn a_later_settlement_advances_past_the_reports_stamped_revision() {
        use crate::findings::{ReviewConclusion, ReviewVerdict};

        let mut led = EvidenceLedger::default();
        led.note_child_settlement("agent-1");
        led.set_review_verdict(ReviewVerdict {
            conclusion: ReviewConclusion::IssuesFound,
            coverage: Vec::new(),
            limitations: Vec::new(),
            report: Some("first".into()),
            report_revision: None,
            evidence_revision: None,
        });
        assert_eq!(
            led.review.as_ref().and_then(|v| v.evidence_revision),
            Some(1)
        );
        led.note_child_settlement("agent-2");
        assert!(
            led.evidence_revision > 1,
            "late evidence advances the revision"
        );
    }

    /// The Lead's own structured changes are report evidence too. A finding
    /// recorded after the report, or an adjudication that changes a decision,
    /// must make the report detectably stale.
    #[test]
    fn the_leads_own_finding_changes_advance_past_the_reports_revision() {
        use crate::findings::{
            FindingAdjudication, FindingDisposition, ReviewConclusion, ReviewVerdict,
        };

        let mut led = EvidenceLedger::default();
        let candidate = led.adopt_candidate("a1", "investigator", &child_finding());
        led.set_review_verdict(ReviewVerdict {
            conclusion: ReviewConclusion::IssuesFound,
            coverage: Vec::new(),
            limitations: Vec::new(),
            report: Some("first".into()),
            report_revision: None,
            evidence_revision: None,
        });
        let stamped = led
            .review
            .as_ref()
            .and_then(|verdict| verdict.evidence_revision)
            .unwrap();

        // A new finding the report did not see.
        led.record_finding(FindingKind::Risk, "new evidence", None, None);
        assert!(led.evidence_revision > stamped);

        // An adjudication that changes the decision.
        let before = led.evidence_revision;
        led.adjudicate_finding(
            &candidate,
            FindingAdjudication {
                disposition: FindingDisposition::Adopted,
                reason: Some("confirmed".into()),
                ..FindingAdjudication::default()
            },
        )
        .unwrap();
        assert!(led.evidence_revision > before);

        // Repeating the identical decision is not a new fact.
        let before = led.evidence_revision;
        led.adjudicate_finding(
            &candidate,
            FindingAdjudication {
                disposition: FindingDisposition::Adopted,
                reason: Some("confirmed".into()),
                ..FindingAdjudication::default()
            },
        )
        .unwrap();
        assert_eq!(led.evidence_revision, before);
    }

    fn adopt(led: &mut EvidenceLedger, id: &str) {
        led.adjudicate_finding(
            id,
            crate::findings::FindingAdjudication {
                disposition: crate::findings::FindingDisposition::Adopted,
                reason: Some("verified".into()),
                ..crate::findings::FindingAdjudication::default()
            },
        )
        .unwrap();
    }

    fn merge_into(led: &mut EvidenceLedger, id: &str, target: &str) {
        led.adjudicate_finding(
            id,
            crate::findings::FindingAdjudication {
                disposition: crate::findings::FindingDisposition::Duplicate,
                duplicate_of: Some(target.into()),
                ..crate::findings::FindingAdjudication::default()
            },
        )
        .unwrap();
    }

    /// Four candidates, one merged duplicate: the formal set is f-1, f-2, f-4.
    /// The allocated ids never move to close the gap.
    fn gapped_formal_ledger() -> (EvidenceLedger, Vec<String>) {
        let mut led = EvidenceLedger::default();
        let ids: Vec<String> = (0..4)
            .map(|_| led.adopt_candidate("agent-1", "investigator", &child_finding()))
            .collect();
        adopt(&mut led, &ids[0]);
        adopt(&mut led, &ids[1]);
        merge_into(&mut led, &ids[2], &ids[0]);
        adopt(&mut led, &ids[3]);
        (led, ids)
    }

    /// A gap is part of the identity: adjudication changes the disposition,
    /// never the id or the allocation sequence.
    #[test]
    fn formal_ids_stay_stable_and_gapped_after_adjudication() {
        let (led, ids) = gapped_formal_ledger();
        assert_eq!(ids, vec!["f-1", "f-2", "f-3", "f-4"]);
        assert_eq!(led.formal_finding_ids(), vec!["f-1", "f-2", "f-4"]);
        assert_eq!(
            led.finding("f-4").map(|finding| finding.id.as_str()),
            Some("f-4"),
            "the surviving finding keeps its allocated id"
        );
        assert_eq!(led.next_finding_seq, 4);
        let unique: std::collections::HashSet<&str> = led
            .findings
            .iter()
            .map(|finding| finding.id.as_str())
            .collect();
        assert_eq!(
            unique.len(),
            led.findings.len(),
            "one review never holds two records with the same formal id"
        );
    }

    /// The structured declaration must equal the durable formal set exactly.
    #[test]
    fn report_validation_requires_the_exact_formal_set() {
        let (led, _) = gapped_formal_ledger();
        let report = "## f-1\n\n## f-2\n\n## f-4\n";

        assert!(
            led.validate_review_report(&["f-1".into(), "f-2".into(), "f-4".into()], report)
                .is_ok()
        );

        let missing = led
            .validate_review_report(&["f-1".into(), "f-2".into()], report)
            .unwrap_err();
        assert!(missing.contains("f-4"), "{missing}");
        assert!(missing.contains("formal"), "{missing}");

        let repeated = led
            .validate_review_report(
                &["f-1".into(), "f-2".into(), "f-2".into(), "f-4".into()],
                report,
            )
            .unwrap_err();
        assert!(repeated.contains("f-2"), "{repeated}");
        assert!(repeated.contains("more than once"), "{repeated}");

        let unknown = led
            .validate_review_report(
                &["f-1".into(), "f-2".into(), "f-4".into(), "f-9".into()],
                report,
            )
            .unwrap_err();
        assert!(unknown.contains("f-9"), "{unknown}");
        assert!(unknown.contains("not a finding"), "{unknown}");
    }

    /// A duplicate, rejection or unresolved candidate is evidence, never a
    /// formal finding: it cannot be declared and cannot be cited as one.
    #[test]
    fn report_validation_rejects_non_formal_ids_in_the_set_and_the_body() {
        let (mut led, ids) = gapped_formal_ledger();
        let id = ids[1].clone();
        led.adjudicate_finding(
            &id,
            crate::findings::FindingAdjudication {
                disposition: crate::findings::FindingDisposition::Rejected,
                reason: Some("caller checks the owner".into()),
                ..crate::findings::FindingAdjudication::default()
            },
        )
        .unwrap();
        // Formal set is now f-1, f-4; f-2 rejected, f-3 duplicate.
        let formal = vec!["f-1".to_string(), "f-4".to_string()];

        let declared = led
            .validate_review_report(&["f-1".into(), "f-2".into(), "f-4".into()], "body")
            .unwrap_err();
        assert!(declared.contains("f-2"), "{declared}");
        assert!(declared.contains("not a formal finding"), "{declared}");

        let duplicate = led
            .validate_review_report(&formal, "## f-1\n\nf-3 与 f-1 同根因。\n")
            .unwrap_err();
        assert!(duplicate.contains("f-3"), "{duplicate}");
        assert!(duplicate.contains("duplicate"), "{duplicate}");

        let rejected = led
            .validate_review_report(&formal, "f-2 需要修复。")
            .unwrap_err();
        assert!(rejected.contains("f-2"), "{rejected}");
        assert!(rejected.contains("rejected"), "{rejected}");

        let unknown = led
            .validate_review_report(&formal, "f-9 需要修复。")
            .unwrap_err();
        assert!(unknown.contains("f-9"), "{unknown}");
        assert!(unknown.contains("not a finding"), "{unknown}");

        // The valid body and set still pass.
        assert!(
            led.validate_review_report(&formal, "## f-1\n\n## f-4\n")
                .is_ok()
        );
    }

    /// Quoted code is material, not a reference: an id-shaped string inside a
    /// fence or an inline code span does not become a formal identity.
    #[test]
    fn report_validation_ignores_identifiers_inside_code() {
        let (led, _) = gapped_formal_ledger();
        let formal = vec!["f-1".to_string(), "f-2".to_string(), "f-4".to_string()];
        let body = "## f-1\n\n```rust\nlet id = \"f-9\"; // f-3 is evidence\n```\n\n\
                    Inline `f-8` and `f-3` stay quoted.\n\n## f-2\n\n## f-4\n";
        assert!(led.validate_review_report(&formal, body).is_ok(), "{body}");

        // A bare id is still a reference.
        assert!(led.validate_review_report(&formal, "see f-3").is_err());
        // Identifiers embedded in a larger token are not references.
        assert!(led.validate_review_report(&formal, "xf-9 f-9a").is_ok());
    }

    /// A Lead-authored finding is formal without an adjudication, and can be
    /// reported.
    #[test]
    fn a_lead_authored_finding_is_part_of_the_formal_set() {
        let mut led = EvidenceLedger::default();
        let id = led.record_finding(FindingKind::Correctness, "missing check", None, None);
        assert_eq!(led.formal_finding_ids(), vec![id.as_str()]);
        assert!(
            led.validate_review_report(std::slice::from_ref(&id), &format!("## {id}\n"))
                .is_ok()
        );
    }

    /// A review with no formal findings may submit an empty declaration and a
    /// body that cites none.
    #[test]
    fn a_review_without_formal_findings_accepts_an_empty_set() {
        let mut led = EvidenceLedger::default();
        led.adopt_candidate("agent-1", "investigator", &child_finding());
        assert!(
            led.validate_review_report(&[], "没有正式 Finding。")
                .is_ok()
        );
        assert!(
            led.validate_review_report(&[], "候选 f-1 未采纳。")
                .is_err()
        );
    }
}
