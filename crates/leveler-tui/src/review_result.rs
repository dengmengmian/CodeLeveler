//! The formal report as a conversation message.
//!
//! A settled review's result is the durable report the reviewer submitted. It
//! is delivered as one ordinary conversation entry at the moment the review
//! finished, so the reader sees it where they are — not as a card far above
//! them that must be opened.
//!
//! The body is composed from the durable row and rendered through the same
//! Markdown path a final assistant answer uses. Composition is deterministic:
//! no model is called, no prose is summarized, and no finding is dropped. The
//! structured `UiReviewFinding` records are the only identities; ids that
//! appear inside the reviewer's narrative stay narrative.

use leveler_client_protocol::{
    UiReviewFinding, UiReviewFindingDisposition, UiReviewReport, UiReviewSummary,
};

use crate::i18n::UiText;
use crate::render::transcript_lines::{
    review_conclusion_label, review_coverage_label, review_limitation_kind_label,
    review_scope_label, review_status_label,
};
use crate::render::{confidence_label, dimension_label, severity_label};

/// Compose the conversation message for one settled review.
///
/// `report` is the durable projection, not a reconstruction from live events.
/// When it is absent the message still states the terminal truth: what ran,
/// how it ended and why there is no report body. It never claims success the
/// reviewer did not reach.
pub fn compose(summary: &UiReviewSummary, report: Option<&UiReviewReport>, t: &UiText) -> String {
    let review = &t.review;
    let mut out = String::new();
    out.push_str(&format!("# {}\n\n", review.report_title));

    let mut facts: Vec<String> = Vec::new();
    facts.push(format!(
        "{}: {}",
        review.scope,
        review_scope_label(&summary.scope, t)
    ));
    facts.push(format!(
        "{}: {}",
        review.status,
        review_status_label(summary.status, t)
    ));
    facts.push(format!(
        "{}: {}",
        review.verdict_section,
        conclusion(summary, report, t)
    ));
    facts.push(counts_line(summary, t));
    facts.push(format!("{}: {}", review.id, summary.id));
    if !summary.model.is_empty() {
        let usage = review
            .usage
            .replacen("{}", &summary.usage.input_tokens.to_string(), 1)
            .replacen("{}", &summary.usage.output_tokens.to_string(), 1)
            .replacen("{}", &summary.usage.cached_input_tokens.to_string(), 1);
        facts.push(format!("{}: {} · {usage}", review.model, summary.model));
    }
    if let Some(finished) = summary.finished_at.as_deref() {
        facts.push(format!(
            "{}: {} → {finished}",
            review.time, summary.created_at
        ));
    }
    if summary.modified_source {
        facts.push(review.modified_source.to_string());
    }
    if summary.object_changed {
        facts.push(review.object_changed.to_string());
    }
    if summary.report_stale {
        facts.push(review.report_stale.to_string());
    }
    match summary.report_source {
        leveler_client_protocol::UiReviewReportSource::Submitted => {}
        leveler_client_protocol::UiReviewReportSource::None => {
            facts.push(review.report_source_none.to_string());
        }
        leveler_client_protocol::UiReviewReportSource::Legacy => {
            facts.push(review.report_source_legacy.to_string());
        }
    }
    for fact in facts {
        out.push_str(&format!("- {fact}\n"));
    }
    if let Some(detail) = summary
        .failure
        .as_deref()
        .or(summary.stop_detail.as_deref())
        .filter(|detail| !detail.trim().is_empty())
    {
        out.push_str(&format!("\n> {detail}\n"));
    }
    out.push('\n');

    let Some(report) = report else {
        out.push_str(&format!("## {}\n\n", review.report_section));
        out.push_str(&format!("{}\n", review.no_formal_findings));
        return out;
    };

    // The structured verdict first: what the reviewer concluded, what it
    // actually covered, and what stopped it. Empty declarations say so.
    out.push_str(&format!("## {}\n\n", review.verdict_section));
    out.push_str(&format!(
        "- {}\n",
        review_conclusion_label(report.verdict.conclusion, t)
    ));
    out.push_str(&format!("- {}\n\n", review.verdict_note));

    out.push_str(&format!("### {}\n\n", review.coverage_section));
    if report.verdict.coverage.is_empty() {
        out.push_str(&format!("- {}\n\n", review.coverage_undeclared));
    } else {
        for coverage in &report.verdict.coverage {
            let status = review_coverage_label(coverage.status, t);
            let dimension = dimension_label(coverage.dimension, t);
            let mut line = format!("- {dimension}: {status}");
            if let Some(scope) = coverage
                .scope
                .as_deref()
                .filter(|scope| !scope.trim().is_empty())
            {
                line.push_str(&format!(" · {}: {scope}", review.scope));
            }
            if !coverage.evidence_refs.is_empty() {
                line.push_str(&format!(
                    " · {}: {}",
                    review.evidence,
                    coverage.evidence_refs.join(", ")
                ));
            }
            if let Some(limitation) = coverage
                .limitations
                .as_deref()
                .filter(|limitation| !limitation.trim().is_empty())
            {
                line.push_str(&format!(" · {}: {limitation}", review.limitations_section));
            }
            out.push_str(&line);
            out.push('\n');
        }
        out.push('\n');
    }
    if !report.verdict.limitations.is_empty() {
        out.push_str(&format!("### {}\n\n", review.limitations_section));
        for limitation in &report.verdict.limitations {
            let source = match limitation.source {
                leveler_client_protocol::UiReviewLimitationSource::Runtime => {
                    review.limitation_source_runtime
                }
                leveler_client_protocol::UiReviewLimitationSource::Reviewer => {
                    review.limitation_source_reviewer
                }
            };
            let kind = review_limitation_kind_label(limitation.kind, t);
            out.push_str(&format!("- [{source}/{kind}] {}\n", limitation.detail));
        }
        out.push('\n');
    }

    let formal: Vec<&UiReviewFinding> = report
        .findings
        .iter()
        .filter(|finding| {
            matches!(
                finding.disposition,
                None | Some(UiReviewFindingDisposition::Adopted)
            )
        })
        .collect();
    let decided: Vec<&UiReviewFinding> = report
        .findings
        .iter()
        .filter(|finding| {
            !matches!(
                finding.disposition,
                None | Some(UiReviewFindingDisposition::Adopted)
            )
        })
        .collect();

    out.push_str(&format!("## {}\n\n", review.findings_section));
    if formal.is_empty() {
        out.push_str(&format!(
            "{}\n\n",
            if decided.is_empty() {
                review.no_findings_detail
            } else {
                review.no_formal_findings
            }
        ));
    } else {
        out.push_str(&format!("{}\n\n", review.findings_identity_note));
        for finding in formal {
            finding_markdown(&mut out, finding, report, t);
        }
    }

    if !decided.is_empty() {
        out.push_str(&format!("## {}\n\n", review.candidates_section));
        for finding in decided {
            let disposition = match finding.disposition {
                Some(UiReviewFindingDisposition::Candidate) => {
                    review.disposition_candidate.to_string()
                }
                Some(UiReviewFindingDisposition::Rejected) => {
                    review.disposition_rejected.to_string()
                }
                Some(UiReviewFindingDisposition::Duplicate) => format_duplicate(
                    review.disposition_duplicate,
                    finding.duplicate_of.as_deref().unwrap_or("-"),
                ),
                _ => String::new(),
            };
            out.push_str(&format!(
                "- [{disposition}] {} ({}){}{}\n",
                finding.title,
                finding.id,
                if finding.source_child.is_empty() {
                    String::new()
                } else {
                    format!(
                        " · {}",
                        review.finding_source.replacen(
                            "{}",
                            &source_title(report, &finding.source_child),
                            1
                        )
                    )
                },
                finding
                    .disposition_reason
                    .as_deref()
                    .filter(|reason| !reason.trim().is_empty())
                    .map(|reason| format!(
                        "\n  - {}",
                        review.disposition_reason.replacen("{}", reason.trim(), 1)
                    ))
                    .unwrap_or_default(),
            ));
        }
        out.push('\n');
    }

    out.push_str(&format!("## {}\n\n", review.report_section));
    if report.report.trim().is_empty() {
        out.push_str("—\n");
        if !report.final_message.trim().is_empty() {
            out.push_str(&format!("\n### {}\n\n", review.final_message_section));
            out.push_str(report.final_message.trim());
            out.push('\n');
        }
    } else {
        out.push_str(report.report.trim());
        out.push('\n');
    }
    out
}

fn counts_line(summary: &UiReviewSummary, t: &UiText) -> String {
    let review = &t.review;
    let counts = &summary.counts;
    let mut facts = format!(
        "P0 {} P1 {} P2 {} P3 {}",
        counts.p0, counts.p1, counts.p2, counts.p3
    );
    if counts.unclassified > 0 {
        facts.push_str(" · ");
        facts.push_str(
            &review
                .unclassified
                .replacen("{}", &counts.unclassified.to_string(), 1),
        );
    }
    if counts.observations > 0 {
        facts.push_str(" · ");
        facts.push_str(
            &review
                .observations
                .replacen("{}", &counts.observations.to_string(), 1),
        );
    }
    let total = counts.total();
    facts.push_str(" · ");
    facts.push_str(&review.findings.replacen("{}", &total.to_string(), 1));
    if counts.pending_candidates > 0 {
        facts.push_str(" · ");
        facts.push_str(&review.pending_candidates.replacen(
            "{}",
            &counts.pending_candidates.to_string(),
            1,
        ));
    }
    facts
}

fn conclusion(summary: &UiReviewSummary, report: Option<&UiReviewReport>, t: &UiText) -> String {
    let declared = report
        .and_then(|report| report.verdict.conclusion)
        .or(summary.conclusion);
    review_conclusion_label(declared, t).to_string()
}

fn finding_markdown(
    out: &mut String,
    finding: &UiReviewFinding,
    report: &UiReviewReport,
    t: &UiText,
) {
    let review = &t.review;
    let mut label = String::new();
    if let Some(severity) = finding.severity {
        label.push_str(&severity_label(severity));
    }
    if let Some(dimension) = finding.dimension {
        label.push_str(&format!(" · {}", dimension_label(dimension, t)));
    }
    if let Some(confidence) = finding.confidence {
        label.push_str(&format!(" · {}", confidence_label(confidence)));
    }
    if finding.disposition == Some(UiReviewFindingDisposition::Adopted) {
        label.push_str(&format!(" · {}", review.disposition_adopted));
    }
    if finding.kind == "observation" {
        label.push_str(&format!(" · {}", review.observation));
    }
    let title = if label.is_empty() {
        format!("{} · {}", finding.id, finding.title)
    } else {
        format!("{} · {label} · {}", finding.id, finding.title)
    };
    out.push_str(&format!("### {title}\n\n"));
    if let Some(file) = finding.file.as_deref().filter(|file| !file.is_empty()) {
        match finding.line {
            Some(line) => out.push_str(&format!("- {file}:{line}\n")),
            None => out.push_str(&format!("- {file}\n")),
        }
    }
    if !finding.source_child.is_empty() {
        out.push_str(&format!(
            "- {}\n",
            review
                .finding_source
                .replacen("{}", &source_title(report, &finding.source_child), 1)
        ));
    }
    if finding.summary.trim() != finding.title.trim() && !finding.summary.trim().is_empty() {
        out.push_str(&format!("- {}\n", finding.summary.trim()));
    }
    for (label, value) in [
        (review.evidence, finding.evidence.as_deref()),
        (review.impact, finding.impact.as_deref()),
        (review.recommendation, finding.recommendation.as_deref()),
    ] {
        if let Some(value) = value.filter(|value| !value.trim().is_empty()) {
            out.push_str(&format!("- {label}: {}\n", value.trim()));
        }
    }
    out.push_str(&format!(
        "- {}\n\n",
        if finding.reproduced {
            review.reproduced
        } else {
            review.not_reproduced
        }
    ));
}

/// The investigator's display title for attribution, falling back to the id
/// when the report carries no matching investigation.
fn source_title(report: &UiReviewReport, source_child: &str) -> String {
    report
        .summary
        .investigations
        .iter()
        .find(|investigation| investigation.id == source_child)
        .map(|investigation| investigation.title.clone())
        .unwrap_or_else(|| source_child.to_string())
}

fn format_duplicate(template: &str, target: &str) -> String {
    template.replacen("{}", target, 1)
}

#[cfg(test)]
mod tests {
    use super::*;
    use leveler_client_protocol::{
        UiReviewCounts, UiReviewScope, UiReviewSeverity, UiReviewStatus,
    };

    fn text() -> &'static UiText {
        crate::i18n::Locale::Zh.text()
    }

    fn summary() -> UiReviewSummary {
        UiReviewSummary {
            id: "review-1".into(),
            session_id: leveler_core::SessionId::new("s1"),
            focus: "检查".into(),
            scope: UiReviewScope::Workspace,
            model: "m".into(),
            status: UiReviewStatus::Completed,
            created_at: "2026-11-18T00:00:00Z".into(),
            finished_at: Some("2026-11-18T00:09:54Z".into()),
            counts: UiReviewCounts {
                p2: 1,
                p3: 18,
                observations: 3,
                ..Default::default()
            },
            conclusion: Some(leveler_client_protocol::UiReviewConclusion::IssuesFound),
            object_changed: false,
            modified_source: false,
            failure: None,
            stop_detail: None,
            usage: Default::default(),
            investigations: Vec::new(),
            report_source: leveler_client_protocol::UiReviewReportSource::Submitted,
            report_evidence_revision: 1,
            evidence_revision: 1,
            report_revision: 1,
            report_stale: false,
        }
    }

    fn finding(id: &str, title: &str, severity: UiReviewSeverity) -> UiReviewFinding {
        UiReviewFinding {
            id: id.into(),
            title: title.into(),
            summary: format!("{title} summary"),
            dimension: Some(leveler_client_protocol::UiReviewDimension::Code),
            severity: Some(severity),
            confidence: Some(leveler_client_protocol::UiReviewConfidence::High),
            kind: "correctness".into(),
            source_child: String::new(),
            file: Some("src/a.rs".into()),
            line: Some(7),
            symbol: None,
            evidence: Some("the call chain".into()),
            impact: Some("users are affected".into()),
            recommendation: Some("restore the check".into()),
            reproduced: false,
            disposition: None,
            disposition_reason: None,
            duplicate_of: None,
            source_finding_id: None,
        }
    }

    #[test]
    fn the_body_carries_every_formal_finding_and_the_report_body() {
        let t = text();
        let summary = summary();
        let report = UiReviewReport {
            summary: summary.clone(),
            object: Default::default(),
            findings: vec![
                finding("f-1", "缺少 owner 过滤", UiReviewSeverity::P2),
                finding("f-2", "另一个 P3", UiReviewSeverity::P3),
                UiReviewFinding {
                    disposition: Some(UiReviewFindingDisposition::Rejected),
                    disposition_reason: Some("证据不足".into()),
                    ..finding("f-9", "候选", UiReviewSeverity::P1)
                },
            ],
            verdict: Default::default(),
            report: "# 审查结论\n\n正式报告正文".into(),
            final_message: String::new(),
        };
        let body = compose(&summary, Some(&report), t);
        for needle in [
            "f-1",
            "f-2",
            "缺少 owner 过滤",
            "另一个 P3",
            "the call chain",
            "restore the check",
            "正式报告正文",
        ] {
            assert!(body.contains(needle), "missing {needle}: {body}");
        }
        assert!(
            !body.contains("f-9\n") && body.contains("候选") && body.contains("证据不足"),
            "a rejected candidate stays visible but is not in the formal list: {body}"
        );
        assert!(
            body.contains("正式 Finding 21") || body.contains("正式 Finding 19"),
            "the counts line states the formal total: {body}"
        );
    }

    #[test]
    fn a_missing_report_states_the_terminal_without_inventing_one() {
        let t = text();
        let mut summary = summary();
        summary.status = UiReviewStatus::Failed;
        summary.failure = Some("provider failed".into());
        let body = compose(&summary, None, t);
        assert!(body.contains("审查失败"), "{body}");
        assert!(body.contains("provider failed"), "{body}");
        assert!(
            body.contains("没有正式 Finding") || body.contains("未提交"),
            "{body}"
        );
    }
}
