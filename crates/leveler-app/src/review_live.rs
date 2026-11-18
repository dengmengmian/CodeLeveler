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
                    Some(count) => format!("{count} formal finding(s)"),
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
         one already finished. Read-only: it never starts, stops or changes a \
         review. The formal report itself is a separate durable record — this \
         tool reports state, not the report body."
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
            "properties": {},
            "additionalProperties": false
        })
    }

    async fn execute(
        &self,
        _input: serde_json::Value,
        _context: ToolContext,
        cancellation: CancellationToken,
    ) -> Result<ToolOutput, ToolError> {
        if cancellation.is_cancelled() {
            return Ok(ToolOutput::error("cancelled before reading the reviews"));
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
fn formal_finding_count(findings_json: &str) -> Option<usize> {
    serde_json::from_str::<serde_json::Value>(findings_json)
        .ok()?
        .as_array()
        .map(Vec::len)
}
