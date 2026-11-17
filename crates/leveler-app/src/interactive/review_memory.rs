//! `memory_search`: the read-only memory query a manual reviewer may use.
//!
//! The ordinary agent receives the bounded `MEMORY.md` index through its
//! prompt and reads topics with the file tools. A reviewer needs the same
//! facts, but it also needs to ask about a SPECIFIC business question while
//! it investigates. This tool exposes only that read: it returns bounded
//! excerpts from the existing Markdown store and never writes, captures,
//! consolidates or changes a rollout.
//!
//! It is registered on the review engine only. The runtime is bound to this
//! process's workspace, so a query cannot name another project's memory.

use async_trait::async_trait;
use leveler_execution::RiskLevel;
use leveler_tools::tool::{Tool, ToolContext, ToolError, ToolOutput};
use tokio_util::sync::CancellationToken;

use crate::memory::MemoryRuntime;

pub(super) const MEMORY_SEARCH_TOOL: &str = "memory_search";

const DEFAULT_LIMIT: usize = 3;
const MAX_LIMIT: usize = 5;
const MAX_QUERY_BYTES: usize = 1024;

pub(super) struct ReviewMemoryTool {
    runtime: MemoryRuntime,
    session_scope: String,
}

impl ReviewMemoryTool {
    pub(super) fn new(runtime: MemoryRuntime, session_scope: String) -> Self {
        Self {
            runtime,
            session_scope,
        }
    }
}

#[derive(Debug, serde::Deserialize)]
struct Args {
    query: String,
    #[serde(default)]
    limit: Option<usize>,
}

#[async_trait]
impl Tool for ReviewMemoryTool {
    fn name(&self) -> &str {
        MEMORY_SEARCH_TOOL
    }

    fn description(&self) -> &str {
        "Search cross-session memory for historical notes relevant to a business \
         question. Read-only and bounded: it returns a few topic excerpts with their \
         paths and modification times. Memory is historical context, not a current \
         fact: read the full topic with `read_file`, and verify every claim against \
         the source, tests or the user's current request. A miss does NOT mean no \
         rule exists."
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
                "query": {
                    "type": "string",
                    "description": "The business rule, decision or entity you are investigating."
                },
                "limit": {
                    "type": "integer",
                    "minimum": 1,
                    "maximum": MAX_LIMIT,
                    "description": "Maximum topic excerpts to return (default 3)."
                }
            },
            "required": ["query"]
        })
    }

    async fn execute(
        &self,
        input: serde_json::Value,
        _context: ToolContext,
        _cancellation: CancellationToken,
    ) -> Result<ToolOutput, ToolError> {
        let args: Args =
            serde_json::from_value(input).map_err(|error| ToolError::InvalidArguments {
                tool: MEMORY_SEARCH_TOOL.to_string(),
                message: error.to_string(),
            })?;
        let query = args.query.trim();
        if query.is_empty() {
            return Ok(ToolOutput::error(
                "memory_search requires a non-empty `query`",
            ));
        }
        if query.len() > MAX_QUERY_BYTES {
            return Ok(ToolOutput::error(format!(
                "memory_search query exceeds {MAX_QUERY_BYTES} bytes"
            )));
        }
        let limit = args.limit.unwrap_or(DEFAULT_LIMIT).clamp(1, MAX_LIMIT);
        let budget = leveler_memory::recall::RecallBudget {
            max_topics: limit,
            ..leveler_memory::recall::RecallBudget::default()
        };
        let hits = match self
            .runtime
            .review_recall(Some(&self.session_scope), query, budget)
            .await
        {
            Ok(hits) => hits,
            Err(error) => {
                return Ok(ToolOutput::error(format!(
                    "memory is unavailable for this review: {error}"
                )));
            }
        };
        if hits.is_empty() {
            return Ok(ToolOutput::ok(format!(
                "No memory topic matched `{query}`. This is not evidence that no rule or \
                 historical decision exists; use the source, documentation and the user's \
                 request to establish the requirement.\n"
            )));
        }
        let mut body = format!(
            "Memory recall for `{query}` (historical context, not a current repository fact):\n\n"
        );
        for hit in &hits {
            let scope = match hit.scope {
                leveler_memory::MemoryScope::Workspace => "workspace",
                leveler_memory::MemoryScope::Global => "global",
            };
            body.push_str(&format!(
                "- [{scope}] `{}` — {} ({} matched line(s))\n",
                hit.relative_path, hit.title, hit.matched_lines
            ));
            if let Some(modified) = hit.modified {
                let modified = chrono::DateTime::<chrono::Utc>::from(modified).to_rfc3339();
                body.push_str(&format!("  modified: {modified}\n"));
            }
            body.push_str("  ```\n");
            for line in hit.excerpt.lines() {
                body.push_str(&format!("  {line}\n"));
            }
            body.push_str("  ```\n");
            if hit.truncated {
                body.push_str("  (excerpt truncated; read the topic for the rest)\n");
            }
            body.push_str(&format!(
                "  full topic: {}\n\n",
                hit.absolute_path.display()
            ));
        }
        body.push_str(
            "Verify a remembered rule against the user's current request, the source and the \
             tests. If the memory conflicts with current evidence, keep the conflict visible \
             instead of choosing silently.\n",
        );
        Ok(ToolOutput::ok(body))
    }
}
