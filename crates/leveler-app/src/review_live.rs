//! `review_status`: what this session's reviews are doing right now.
//!
//! A manual review is not a turn, so nothing in the turn table answers "is it
//! still running, and what is it on". The live half comes from the runtime's
//! running-review registry (the same fact `ReviewProgress` carries); the
//! settled half comes from the durable review rows. Both are this session's
//! own record — the tool cannot name another session, a path or a project.
//!
//! Read-only: it never starts, stops or re-drives a review.

use async_trait::async_trait;
use leveler_client_protocol::UiReviewStatus;
use leveler_execution::RiskLevel;
use leveler_storage::{Database, ReviewRepository};
use leveler_tools::tool::{Tool, ToolContext, ToolError, ToolOutput};
use tokio_util::sync::CancellationToken;

use leveler_core::SessionId;

use crate::interactive::LiveReviews;

/// The tool name. Kept in one place so the allowlist and the tool agree.
pub(crate) const REVIEW_STATUS_TOOL: &str = "review_status";

/// One review that is running right now.
#[derive(Clone, Debug)]
pub(crate) struct LiveReview {
    pub summary: leveler_client_protocol::UiReviewSummary,
    /// The reviewer's most recent tool, when it reported one.
    pub tool: Option<String>,
    pub elapsed_ms: u64,
    pub input_tokens: u32,
    pub output_tokens: u32,
    pub cached_input_tokens: u32,
}

/// How many settled rows the answer lists at most.
const SETTLED_MAX: usize = 5;
/// Total answer bound. A status read is context, not a report.
const ANSWER_MAX_BYTES: usize = 12 * 1024;

pub(crate) struct ReviewStatusTool {
    live: LiveReviews,
    db: Database,
    session_id: SessionId,
}

impl ReviewStatusTool {
    pub(crate) fn new(live: LiveReviews, db: Database, session_id: SessionId) -> Self {
        Self {
            live,
            db,
            session_id,
        }
    }

    fn running(&self) -> Vec<LiveReview> {
        let guard = self
            .live
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut rows: Vec<LiveReview> = guard
            .get(&self.session_id)
            .map(|reviews| reviews.values().cloned().collect())
            .unwrap_or_default();
        rows.sort_by(|a, b| a.summary.created_at.cmp(&b.summary.created_at));
        rows
    }

    async fn settled(&self) -> Result<Vec<String>, ToolError> {
        let records = ReviewRepository::new(&self.db)
            .list_for_session(&self.session_id)
            .await
            .map_err(|error| {
                ToolError::Io(format!("could not read this session's reviews: {error}"))
            })?;
        Ok(records
            .into_iter()
            .filter(|record| record.status != UiReviewStatus::Running.as_str())
            .take(SETTLED_MAX)
            .map(|record| {
                let findings = match formal_finding_count(&record.findings_json) {
                    Some((formal, pending)) if pending > 0 => {
                        format!("{formal} formal finding(s), {pending} pending candidate(s)")
                    }
                    Some((formal, _)) => format!("{formal} formal finding(s)"),
                    None => "formal findings unreadable".to_string(),
                };
                let report = if record.report_source == "submitted" {
                    "report submitted"
                } else {
                    "no formal report"
                };
                format!(
                    "- {} · {} · {} · {findings} · {report}",
                    record.id, record.status, record.scope_json
                )
            })
            .collect())
    }

    /// One settled review's durable formal report.
    ///
    /// The row is the authority: findings keep the ledger's ids, and the
    /// submitted report body is quoted verbatim. A review that submitted no
    /// report says so; the reviewer's last message is labelled as what it is.
    async fn report(
        &self,
        review_id: &str,
        cancellation: &CancellationToken,
    ) -> Result<ToolOutput, ToolError> {
        if cancellation.is_cancelled() {
            return Ok(ToolOutput::error("cancelled before reading the report"));
        }
        let Some(record) = ReviewRepository::new(&self.db)
            .get(review_id)
            .await
            .map_err(|error| {
                ToolError::Io(format!("could not read review {review_id}: {error}"))
            })?
        else {
            return Ok(ToolOutput::error(format!(
                "no review `{review_id}` in this session"
            )));
        };
        if record.session_id != self.session_id {
            return Ok(ToolOutput::error(format!(
                "review `{review_id}` does not belong to this session"
            )));
        }
        let summary =
            crate::interactive::review::summary_from_record(&record).map_err(|error| {
                ToolError::Io(format!("could not project review {review_id}: {error}"))
            })?;
        let report = crate::interactive::review::report_from_record(&record).ok();

        let mut text = format!("## Review {}\n\n", summary.id);
        text.push_str(&format!("- status: {}\n", summary.status.as_str()));
        text.push_str(&format!("- scope: {}\n", scope_label(&summary.scope)));
        if !summary.focus.trim().is_empty() {
            text.push_str(&format!("- focus: {}\n", summary.focus.trim()));
        }
        if let Some(conclusion) = summary.conclusion {
            text.push_str(&format!("- conclusion: {}\n", conclusion.as_str()));
        }
        text.push_str(&format!(
            "- counts: p0={} p1={} p2={} p3={} unclassified={} observations={} pending_candidates={}\n",
            summary.counts.p0,
            summary.counts.p1,
            summary.counts.p2,
            summary.counts.p3,
            summary.counts.unclassified,
            summary.counts.observations,
            summary.counts.pending_candidates
        ));
        if let Some(finished) = summary.finished_at.as_deref() {
            text.push_str(&format!("- times: {} → {finished}\n", summary.created_at));
        }
        if let Some(failure) = summary
            .failure
            .as_deref()
            .or(summary.stop_detail.as_deref())
            .filter(|detail| !detail.trim().is_empty())
        {
            text.push_str(&format!("- stop: {failure}\n"));
        }
        if summary.report_stale {
            text.push_str(
                "- report_stale: the submitted report predates the latest structured evidence\n",
            );
        }

        if let Some(report) = report {
            let formal: Vec<&leveler_client_protocol::UiReviewFinding> = report
                .findings
                .iter()
                .filter(|finding| {
                    matches!(
                        finding.disposition,
                        None | Some(leveler_client_protocol::UiReviewFindingDisposition::Adopted)
                    )
                })
                .collect();
            let decided: Vec<&leveler_client_protocol::UiReviewFinding> = report
                .findings
                .iter()
                .filter(|finding| {
                    !matches!(
                        finding.disposition,
                        None | Some(leveler_client_protocol::UiReviewFindingDisposition::Adopted)
                    )
                })
                .collect();
            text.push_str(
                "- formal_finding_identity: only the ids under `Formal findings` are formal \
                 findings of this review. Id tokens inside the report body are prose; never map \
                 a missing id onto another finding.\n",
            );
            text.push_str("\n### Formal findings\n");
            if formal.is_empty() {
                text.push_str("None.\n");
            }
            for finding in &formal {
                text.push_str(&format!(
                    "- {} · {} · {}\n",
                    finding.id,
                    severity_label(finding.severity),
                    finding.title.trim()
                ));
                if let Some(file) = finding.file.as_deref().filter(|file| !file.is_empty()) {
                    match finding.line {
                        Some(line) => text.push_str(&format!("  location: {file}:{line}\n")),
                        None => text.push_str(&format!("  location: {file}\n")),
                    }
                }
                if finding.summary.trim() != finding.title.trim()
                    && !finding.summary.trim().is_empty()
                {
                    text.push_str(&format!("  summary: {}\n", finding.summary.trim()));
                }
                for (label, value) in [
                    ("evidence", finding.evidence.as_deref()),
                    ("impact", finding.impact.as_deref()),
                    ("recommendation", finding.recommendation.as_deref()),
                ] {
                    if let Some(value) = value.filter(|value| !value.trim().is_empty()) {
                        text.push_str(&format!("  {label}: {}\n", value.trim()));
                    }
                }
            }
            if !decided.is_empty() {
                text.push_str("\n### Candidates and dispositions (not formal findings)\n");
                for finding in &decided {
                    text.push_str(&format!(
                        "- {} · {} · {} · {}\n",
                        finding.id,
                        finding
                            .disposition
                            .map(|disposition| disposition.as_str())
                            .unwrap_or("unclassified"),
                        finding.title.trim(),
                        finding.disposition_reason.as_deref().unwrap_or("")
                    ));
                }
            }
            text.push_str("\n### Coverage\n");
            if report.verdict.coverage.is_empty() {
                text.push_str("The reviewer declared no coverage.\n");
            }
            for coverage in &report.verdict.coverage {
                text.push_str(&format!(
                    "- {}: {}",
                    crate::interactive::review::review_dimension_name(coverage.dimension),
                    coverage.status.as_str()
                ));
                if let Some(scope) = coverage.scope.as_deref() {
                    text.push_str(&format!(" · {scope}"));
                }
                if !coverage.evidence_refs.is_empty() {
                    text.push_str(&format!(
                        " · evidence: {}",
                        coverage.evidence_refs.join(", ")
                    ));
                }
                text.push('\n');
            }
            text.push_str("\n### Limitations\n");
            if report.verdict.limitations.is_empty() {
                text.push_str("None declared.\n");
            }
            for limitation in &report.verdict.limitations {
                text.push_str(&format!(
                    "- [{}] {}\n",
                    limitation.kind.as_str(),
                    limitation.detail
                ));
            }
            text.push_str("\n### Submitted report\n");
            if report.report.trim().is_empty() {
                text.push_str(
                    "No formal report body was submitted. The reviewer's last message is NOT \
                     the formal report.\n",
                );
                if !report.final_message.trim().is_empty() {
                    text.push_str("\n#### Last message\n");
                    text.push_str(report.final_message.trim());
                    text.push('\n');
                }
            } else {
                text.push_str(report.report.trim());
                text.push('\n');
            }
        } else {
            text.push_str(
                "\nThe durable report projection is unavailable for this row; its status and \
                 counts above are the facts this session holds.\n",
            );
        }
        Ok(ToolOutput::ok(leveler_core::truncate_head_bytes(
            &text,
            REPORT_ANSWER_MAX_BYTES,
            "\n… (report truncated at the tool bound)\n",
        )))
    }
}

/// A report read is context the model asked for, not a status glance. The
/// report body itself is capped at 64 KiB by `report_review`, so this bound
/// leaves room for the structured findings around it.
const REPORT_ANSWER_MAX_BYTES: usize = 128 * 1024;

fn severity_label(severity: Option<leveler_client_protocol::UiReviewSeverity>) -> &'static str {
    use leveler_client_protocol::UiReviewSeverity;
    match severity {
        Some(UiReviewSeverity::P0) => "P0",
        Some(UiReviewSeverity::P1) => "P1",
        Some(UiReviewSeverity::P2) => "P2",
        Some(UiReviewSeverity::P3) => "P3",
        None => "unclassified",
    }
}

#[async_trait]
impl Tool for ReviewStatusTool {
    fn name(&self) -> &str {
        REVIEW_STATUS_TOOL
    }

    fn description(&self) -> &str {
        "Read the state of this session's manual reviews. `running` reviews are \
         live facts (status, reviewed object, elapsed time, current tool, and \
         each independent investigation); `settled` lists the durable result \
         rows. Use it to answer what a review is doing right now, or whether \
         one already finished. Pass `review_id` to read that settled review's \
         durable formal report — its findings, evidence, coverage, limitations \
         and submitted report body — so you can explain or act on specific \
         findings. Read-only: it never starts, stops or changes a review."
    }

    fn requires_workspace(&self) -> bool {
        false
    }

    fn risk(&self) -> RiskLevel {
        RiskLevel::Safe
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "review_id": {
                    "type": "string",
                    "description": "A review id from this session's settled list. Returns that review's durable formal report."
                }
            },
            "additionalProperties": false
        })
    }

    async fn execute(
        &self,
        input: serde_json::Value,
        _context: ToolContext,
        cancellation: CancellationToken,
    ) -> Result<ToolOutput, ToolError> {
        if cancellation.is_cancelled() {
            return Ok(ToolOutput::error("cancelled before reading the reviews"));
        }
        let review_id = input
            .get("review_id")
            .and_then(serde_json::Value::as_str)
            .map(str::trim)
            .filter(|id| !id.is_empty());
        if let Some(review_id) = review_id {
            return self.report(review_id, &cancellation).await;
        }
        let running = self.running();
        let settled = self.settled().await?;
        if running.is_empty() && settled.is_empty() {
            return Ok(ToolOutput::ok(
                "This session has no manual reviews: none is running and none has settled.",
            ));
        }
        let mut text = String::from("## Reviews in this session\n");
        if running.is_empty() {
            text.push_str("\n### Running\nNone.\n");
        } else {
            text.push_str("\n### Running\n");
            for review in &running {
                let summary = &review.summary;
                text.push_str(&format!(
                    "- {} · running · {} · {}\n",
                    summary.id,
                    scope_label(&review.summary.scope),
                    format_ms(review.elapsed_ms),
                ));
                if !summary.focus.trim().is_empty() {
                    text.push_str(&format!(
                        "  focus: {}\n",
                        leveler_core::truncate_head_bytes(&summary.focus, 500, "…")
                    ));
                }
                if let Some(tool) = &review.tool {
                    text.push_str(&format!("  current tool: {tool}\n"));
                }
                if review.input_tokens > 0 || review.output_tokens > 0 {
                    text.push_str(&format!(
                        "  tokens: {} in / {} out ({} cached)\n",
                        review.input_tokens, review.output_tokens, review.cached_input_tokens
                    ));
                }
                if summary.investigations.is_empty() {
                    text.push_str("  investigations: none started\n");
                } else {
                    text.push_str("  investigations:\n");
                    for investigation in &summary.investigations {
                        text.push_str(&format!(
                            "  - {} · {} · {} candidate(s)\n",
                            investigation.title,
                            investigation_state(investigation.state),
                            investigation.findings
                        ));
                    }
                }
            }
        }
        text.push_str("\n### Settled (newest first)\n");
        if settled.is_empty() {
            text.push_str("None.\n");
        } else {
            for row in &settled {
                text.push_str(row);
                text.push('\n');
            }
        }
        Ok(ToolOutput::ok(leveler_core::truncate_head_bytes(
            &text,
            ANSWER_MAX_BYTES,
            "\n… (truncated)\n",
        )))
    }
}

fn scope_label(scope: &leveler_client_protocol::UiReviewScope) -> String {
    use leveler_client_protocol::UiReviewScope;
    match scope {
        UiReviewScope::Auto => "auto".to_string(),
        UiReviewScope::CurrentTask => "current task".to_string(),
        UiReviewScope::Workspace => "workspace".to_string(),
        UiReviewScope::Project => "project".to_string(),
        UiReviewScope::Session { .. } => "session".to_string(),
        UiReviewScope::Commit { rev } => format!("commit {rev}"),
        UiReviewScope::Branch { base } => format!("branch {base}"),
        UiReviewScope::Paths { paths } => format!("paths {}", paths.join(",")),
    }
}

fn investigation_state(state: leveler_client_protocol::UiReviewInvestigationState) -> &'static str {
    use leveler_client_protocol::UiReviewInvestigationState as State;
    match state {
        State::Running => "running",
        State::Completed => "completed",
        State::Failed => "failed",
        State::Cancelled => "cancelled",
        State::Incomplete => "incomplete",
    }
}

fn format_ms(ms: u64) -> String {
    let seconds = ms / 1000;
    if seconds < 60 {
        format!("{seconds}s")
    } else {
        format!("{}m {:02}s", seconds / 60, seconds % 60)
    }
}

/// How many formal findings a durable row holds. `None` when the row cannot be
/// read as a JSON array — unknown is not zero.
/// The formal-finding count of one stored ledger, and how many Investigator
/// candidates are still undecided.
///
/// The ledger JSON is the authority: a candidate, a rejected record or a
/// duplicate is not a formal finding, and a status answer that counted every
/// record would overstate the review's defects.
fn formal_finding_count(findings_json: &str) -> Option<(usize, usize)> {
    let records = serde_json::from_str::<serde_json::Value>(findings_json)
        .ok()?
        .as_array()
        .cloned()?;
    let mut formal = 0usize;
    let mut pending = 0usize;
    for record in &records {
        match record
            .get("disposition")
            .and_then(serde_json::Value::as_str)
        {
            None | Some("adopted") => formal += 1,
            Some("candidate") => pending += 1,
            _ => {}
        }
    }
    Some((formal, pending))
}

#[cfg(test)]
mod tests {
    use super::*;
    use leveler_core::now;
    use leveler_storage::{NewReview, ReviewFinish, SessionRecord, SessionRepository};
    use leveler_tools::tool::Tool;
    use std::sync::{Arc, Mutex};

    async fn settled_review(report_source: &str, report: &str) -> (Database, SessionId, String) {
        let db = Database::connect_in_memory().await.unwrap();
        let record = SessionRecord::new("/repo", "goal", "m/x", now());
        let session = SessionId::new(record.id.clone());
        SessionRepository::new(&db).create(&record).await.unwrap();
        let id = "review-1";
        ReviewRepository::new(&db)
            .create(&NewReview {
                id,
                session_id: &session,
                focus: "检查账单",
                scope_json: r#"{"kind":"workspace"}"#,
                model: "m/x",
                status: "running",
                source_session_id: Some(session.as_str()),
                source_turn_id: None,
                created_at: now(),
            })
            .await
            .unwrap();
        ReviewRepository::new(&db)
            .finish(
                id,
                &ReviewFinish {
                    status: "completed",
                    object_json: Some(r#"{"workspace":"/repo"}"#),
                    findings_json: "[]",
                    report,
                    report_source,
                    final_message: "",
                    evidence_revision: 1,
                    report_revision: 1,
                    report_evidence_revision: 1,
                    error: None,
                    stop_detail: None,
                    usage_json: "{}",
                    verdict_json: Some(
                        r#"{"conclusion":"no_issues_found","coverage":[],"limitations":[]}"#,
                    ),
                    activity_json: "[]",
                    object_changed: false,
                    modified_source: false,
                    investigations_json: "[]",
                    finished_at: now(),
                },
            )
            .await
            .unwrap();
        (db, session, id.to_string())
    }

    fn context() -> (ToolContext, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let workspace = leveler_execution::Workspace::new(dir.path()).unwrap();
        (
            ToolContext::new(workspace, leveler_execution::PermissionProfile::FullAccess),
            dir,
        )
    }

    /// The ordinary agent can read the durable report body, not only its
    /// status. Without this the parent could cite a finding id from the
    /// settlement notice but could not answer "what does the report say".
    #[tokio::test]
    async fn the_tool_returns_the_settled_formal_report_body() {
        let (db, session, review_id) =
            settled_review("submitted", "正式报告正文：账单读取缺少 owner 过滤。").await;
        let tool = ReviewStatusTool::new(
            Arc::new(Mutex::new(std::collections::HashMap::new())),
            db,
            session,
        );
        let (ctx, _dir) = context();
        let output = tool
            .execute(
                serde_json::json!({ "review_id": review_id }),
                ctx,
                CancellationToken::new(),
            )
            .await
            .unwrap();
        assert!(!output.is_error, "{}", output.content);
        assert!(
            output.content.contains("### Formal findings"),
            "{}",
            output.content
        );
        assert!(
            output.content.contains("### Submitted report"),
            "{}",
            output.content
        );
        assert!(
            output.content.contains("正式报告正文"),
            "{}",
            output.content
        );
        assert!(output.content.contains("no_issues_found") || output.content.contains("状态"));
    }

    /// A row with no formal submission must not turn the reviewer's last chat
    /// message into one.
    #[tokio::test]
    async fn the_tool_labels_a_missing_formal_report_as_missing() {
        let (db, session, review_id) = settled_review("none", "").await;
        let tool = ReviewStatusTool::new(
            Arc::new(Mutex::new(std::collections::HashMap::new())),
            db,
            session,
        );
        let (ctx, _dir) = context();
        let output = tool
            .execute(
                serde_json::json!({ "review_id": review_id }),
                ctx,
                CancellationToken::new(),
            )
            .await
            .unwrap();
        assert!(!output.is_error, "{}", output.content);
        assert!(
            output
                .content
                .contains("No formal report body was submitted"),
            "{}",
            output.content
        );
    }

    /// The state summary must not lend a child's proposal the review's formal
    /// authority: candidates are counted separately and rejected/duplicate
    /// records are neither.
    #[tokio::test]
    async fn the_state_list_counts_only_formal_findings_as_formal() {
        let db = Database::connect_in_memory().await.unwrap();
        let record = SessionRecord::new("/repo", "goal", "m/x", now());
        let session = SessionId::new(record.id.clone());
        SessionRepository::new(&db).create(&record).await.unwrap();
        ReviewRepository::new(&db)
            .create(&NewReview {
                id: "review-1",
                session_id: &session,
                focus: "检查",
                scope_json: r#"{"kind":"workspace"}"#,
                model: "m/x",
                status: "running",
                source_session_id: Some(session.as_str()),
                source_turn_id: None,
                created_at: now(),
            })
            .await
            .unwrap();
        ReviewRepository::new(&db)
            .finish(
                "review-1",
                &ReviewFinish {
                    status: "completed",
                    object_json: None,
                    findings_json: r#"[
                        {"id":"f-1","kind":"correctness","summary":"lead finding"},
                        {"id":"f-2","kind":"correctness","summary":"adopted","disposition":"adopted"},
                        {"id":"f-3","kind":"correctness","summary":"pending","disposition":"candidate"},
                        {"id":"f-4","kind":"correctness","summary":"rejected","disposition":"rejected"},
                        {"id":"f-5","kind":"correctness","summary":"duplicate","disposition":"duplicate"}
                    ]"#,
                    report: "",
                    report_source: "none",
                    final_message: "",
                    evidence_revision: 1,
                    report_revision: 0,
                    report_evidence_revision: 0,
                    error: None,
                    stop_detail: None,
                    usage_json: "{}",
                    verdict_json: None,
                    activity_json: "[]",
                    object_changed: false,
                    modified_source: false,
                    investigations_json: "[]",
                    finished_at: now(),
                },
            )
            .await
            .unwrap();
        let tool = ReviewStatusTool::new(
            Arc::new(Mutex::new(std::collections::HashMap::new())),
            db,
            session,
        );
        let rows = tool.settled().await.unwrap();
        assert_eq!(rows.len(), 1, "{rows:?}");
        assert!(
            rows[0].contains("2 formal finding(s)"),
            "the Lead's finding and the adopted candidate are the formal set: {rows:?}"
        );
        assert!(
            rows[0].contains("1 pending candidate(s)"),
            "an undecided candidate is not a formal finding: {rows:?}"
        );
        assert!(
            !rows[0].contains("5 formal"),
            "rejected/duplicate records are not formal findings: {rows:?}"
        );
    }
}
