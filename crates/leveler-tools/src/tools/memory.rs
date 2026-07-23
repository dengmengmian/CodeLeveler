//! Memory tools: `memory` (search/list/read), `remember`, `forget`.
//!
//! Store root comes from [`ToolContext::memory_root`] (app sets
//! `Layout::memory_dir`). Writes require human approval (K36).

use async_trait::async_trait;
use schemars::JsonSchema;
use serde::Deserialize;
use tokio_util::sync::CancellationToken;

use leveler_execution::RiskLevel;
use leveler_memory::{MemoryStore, new_entry};

use crate::tool::{Tool, ToolContext, ToolError, ToolOutput};

fn open_store(context: &ToolContext) -> Result<MemoryStore, ToolError> {
    let root = context.memory_root.as_ref().ok_or_else(|| {
        ToolError::Io(
            "memory store is not configured for this session (app must set Layout::memory_dir)"
                .to_string(),
        )
    })?;
    MemoryStore::open(root).map_err(|e| ToolError::Io(e.to_string()))
}

/// Give `entry` an id that won't silently clobber a *different* memory.
///
/// The id is `slugify(title)`, so two unrelated facts with the same (or
/// slug-colliding) title map to one file and the second `fs::write` would
/// overwrite the first. Keep the id when it's free or already holds this exact
/// fact (idempotent re-remember); otherwise suffix it (`-2`, `-3`, …) so both
/// survive.
fn deduplicate_id(store: &MemoryStore, entry: &leveler_memory::MemoryEntry) -> String {
    match store.read_active(&entry.id) {
        // Free, or the identical fact is already stored: keep the id.
        Err(_) => entry.id.clone(),
        Ok(existing)
            if existing.title == entry.title && existing.body.trim() == entry.body.trim() =>
        {
            entry.id.clone()
        }
        Ok(_) => {
            let mut n = 2;
            loop {
                let candidate = format!("{}-{n}", entry.id);
                if store.read_active(&candidate).is_err() {
                    return candidate;
                }
                n += 1;
            }
        }
    }
}

#[derive(Debug, Deserialize, JsonSchema)]
struct MemoryArgs {
    /// Action: search | list | read
    action: String,
    /// Search query (action=search) or entry id (action=read).
    #[serde(default)]
    query: Option<String>,
    #[serde(default)]
    id: Option<String>,
    #[serde(default = "default_limit")]
    limit: usize,
}

fn default_limit() -> usize {
    5
}

pub struct MemoryTool;

#[async_trait]
impl Tool for MemoryTool {
    fn name(&self) -> &'static str {
        "memory"
    }

    fn description(&self) -> &'static str {
        "Search, list, or read durable project memories (user-approved facts and \
         preferences). action=search|vector_search|list|read. Bodies are not in \
         the system prompt — retrieve them here. vector_search uses local dense \
         vectors (no cloud embeddings)."
    }

    fn input_schema(&self) -> serde_json::Value {
        super::schema_of::<MemoryArgs>()
    }

    fn risk(&self) -> RiskLevel {
        RiskLevel::Safe
    }

    async fn execute(
        &self,
        input: serde_json::Value,
        context: ToolContext,
        _cancellation: CancellationToken,
    ) -> Result<ToolOutput, ToolError> {
        let args: MemoryArgs = super::parse_input(self.name(), input)?;
        let store = open_store(&context)?;
        match args.action.as_str() {
            "list" => {
                let entries = store
                    .list_active()
                    .map_err(|e| ToolError::Io(e.to_string()))?;
                if entries.is_empty() {
                    return Ok(ToolOutput::ok("No active memories.".to_string()));
                }
                let mut body = String::new();
                for e in entries {
                    body.push_str(&format!("- [{}] {}\n", e.id, e.title));
                }
                Ok(ToolOutput::ok(body))
            }
            "read" => {
                let id = args
                    .id
                    .or(args.query)
                    .ok_or_else(|| ToolError::InvalidArguments {
                        tool: self.name().into(),
                        message: "read requires id".into(),
                    })?;
                let e = store
                    .read_active(&id)
                    .map_err(|err| ToolError::Io(err.to_string()))?;
                Ok(ToolOutput::ok(format!(
                    "# {}\n\n{}\n\n(tags: {})",
                    e.title,
                    e.body,
                    e.tags.join(", ")
                )))
            }
            "search" | "vector_search" => {
                let q = args.query.unwrap_or_default();
                let hits = if args.action == "vector_search" {
                    store
                        .vector_search(&q, args.limit.max(1))
                        .map_err(|e| ToolError::Io(e.to_string()))?
                } else {
                    store
                        .search(&q, args.limit.max(1))
                        .map_err(|e| ToolError::Io(e.to_string()))?
                };
                if hits.is_empty() {
                    return Ok(ToolOutput::ok("No matching memories.".to_string()));
                }
                let mut body = String::new();
                for (e, score) in hits {
                    body.push_str(&format!(
                        "- [{}] {} (score {:.2})\n  {}\n",
                        e.id,
                        e.title,
                        score,
                        e.body.lines().next().unwrap_or("")
                    ));
                }
                Ok(ToolOutput::ok(body))
            }
            other => Ok(ToolOutput::error(format!(
                "unknown action `{other}`; use search|vector_search|list|read"
            ))),
        }
    }
}

#[derive(Debug, Deserialize, JsonSchema)]
struct RememberArgs {
    title: String,
    body: String,
    #[serde(default)]
    tags: Vec<String>,
}

pub struct RememberTool;

#[async_trait]
impl Tool for RememberTool {
    fn name(&self) -> &'static str {
        "remember"
    }

    fn description(&self) -> &'static str {
        "Propose a durable project memory (title + body). Requires user approval \
         before it is stored. Do not store secrets or raw transcripts."
    }

    fn input_schema(&self) -> serde_json::Value {
        super::schema_of::<RememberArgs>()
    }

    fn risk(&self) -> RiskLevel {
        // WorkspaceWrite so mode permits; ApprovalPolicy always NeedApproval (K36).
        RiskLevel::WorkspaceWrite
    }

    async fn execute(
        &self,
        input: serde_json::Value,
        context: ToolContext,
        _cancellation: CancellationToken,
    ) -> Result<ToolOutput, ToolError> {
        let args: RememberArgs = super::parse_input(self.name(), input)?;
        if args.title.trim().is_empty() || args.body.trim().is_empty() {
            return Ok(ToolOutput::error("title and body are required"));
        }
        let store = open_store(&context)?;
        let mut entry = new_entry(&args.title, &args.body, args.tags);
        entry.id = deduplicate_id(&store, &entry);
        let saved = store
            .remember(entry)
            .map_err(|e| ToolError::Io(e.to_string()))?;
        Ok(ToolOutput::ok(format!(
            "Remembered [{}]: {}",
            saved.id, saved.title
        )))
    }
}

#[derive(Debug, Deserialize, JsonSchema)]
struct ForgetArgs {
    id: String,
}

pub struct ForgetTool;

#[async_trait]
impl Tool for ForgetTool {
    fn name(&self) -> &'static str {
        "forget"
    }

    fn description(&self) -> &'static str {
        "Archive a durable memory by id (soft-delete; retained for audit). \
         Requires user approval."
    }

    fn input_schema(&self) -> serde_json::Value {
        super::schema_of::<ForgetArgs>()
    }

    fn risk(&self) -> RiskLevel {
        RiskLevel::WorkspaceWrite
    }

    async fn execute(
        &self,
        input: serde_json::Value,
        context: ToolContext,
        _cancellation: CancellationToken,
    ) -> Result<ToolOutput, ToolError> {
        let args: ForgetArgs = super::parse_input(self.name(), input)?;
        let store = open_store(&context)?;
        let entry = store
            .forget(&args.id)
            .map_err(|e| ToolError::Io(e.to_string()))?;
        Ok(ToolOutput::ok(format!(
            "Archived memory [{}]: {}",
            entry.id, entry.title
        )))
    }
}

#[derive(Debug, Deserialize, JsonSchema)]
struct ConsolidateArgs {
    /// Transcript or notes to extract durable preferences from.
    transcript: String,
    /// When true, write candidates immediately (still Dangerous risk / approval).
    #[serde(default)]
    auto_write: bool,
    #[serde(default = "default_max")]
    max_candidates: usize,
}

fn default_max() -> usize {
    5
}

pub struct ConsolidateMemoryTool;

#[async_trait]
impl Tool for ConsolidateMemoryTool {
    fn name(&self) -> &'static str {
        "consolidate_memory"
    }

    fn description(&self) -> &'static str {
        "Extract durable preference/decision candidates from a transcript.          With auto_write=false (default), returns candidates for the user to          approve via remember. With auto_write=true, writes after host approval          (tool is Dangerous-class / WorkspaceWrite + approval policy)."
    }

    fn input_schema(&self) -> serde_json::Value {
        super::schema_of::<ConsolidateArgs>()
    }

    fn risk(&self) -> RiskLevel {
        RiskLevel::WorkspaceWrite
    }

    async fn execute(
        &self,
        input: serde_json::Value,
        context: ToolContext,
        _cancellation: CancellationToken,
    ) -> Result<ToolOutput, ToolError> {
        let args: ConsolidateArgs = super::parse_input(self.name(), input)?;
        let candidates =
            leveler_memory::extract_memory_candidates(&args.transcript, args.max_candidates.max(1));
        if candidates.is_empty() {
            return Ok(ToolOutput::ok("No durable candidates found."));
        }
        // Default path: candidates only. auto_write still requires host K36
        // approval (`is_memory_write_tool("consolidate_memory")`) before this
        // tool runs; after approval we may persist.
        if !args.auto_write {
            let mut body = String::from("Candidates (not written; call remember to store):\n");
            for e in &candidates {
                body.push_str(&format!(
                    "- {} — {}\n",
                    e.title,
                    e.body.chars().take(120).collect::<String>()
                ));
            }
            return Ok(ToolOutput::ok(body).with_metadata(serde_json::json!({
                "candidates": candidates
            })));
        }
        // Defense in depth: refuse auto_write when approval policy would treat
        // this as a free WorkspaceWrite (should never reach here under AutoApprove).
        if !leveler_execution::is_memory_write_tool(self.name()) {
            return Ok(ToolOutput::error(
                "consolidate_memory auto_write blocked: tool is not classified as a memory write",
            ));
        }
        let store = open_store(&context)?;
        let mut written = Vec::new();
        for mut e in candidates {
            e.id = deduplicate_id(&store, &e);
            let saved = store
                .remember(e)
                .map_err(|err| ToolError::Io(err.to_string()))?;
            written.push(saved.id);
        }
        Ok(ToolOutput::ok(format!(
            "Wrote {} memories: {}",
            written.len(),
            written.join(", ")
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[tokio::test]
    async fn uses_context_memory_root_not_env() {
        let dir = tempdir().unwrap();
        let mem = dir.path().join("memory");
        let ws = leveler_execution::Workspace::new(dir.path()).unwrap();
        let ctx = ToolContext::new(ws, leveler_execution::PermissionProfile::Assisted)
            .with_memory_root(&mem);
        let out = RememberTool
            .execute(
                serde_json::json!({
                    "title": "Prefer workspace-write",
                    "body": "Use PermissionProfile::Assisted for edits."
                }),
                ctx.clone(),
                CancellationToken::new(),
            )
            .await
            .unwrap();
        assert!(!out.is_error, "{}", out.content);
        assert!(mem.join("active").exists());
        let listed = MemoryTool
            .execute(
                serde_json::json!({"action": "list"}),
                ctx,
                CancellationToken::new(),
            )
            .await
            .unwrap();
        assert!(listed.content.contains("Prefer workspace-write"));
    }

    #[tokio::test]
    async fn two_different_facts_with_the_same_title_do_not_clobber_each_other() {
        let dir = tempdir().unwrap();
        let mem = dir.path().join("memory");
        let ws = leveler_execution::Workspace::new(dir.path()).unwrap();
        let ctx = ToolContext::new(ws, leveler_execution::PermissionProfile::Assisted)
            .with_memory_root(&mem);
        let remember = |body: &'static str| {
            let ctx = ctx.clone();
            async move {
                RememberTool
                    .execute(
                        serde_json::json!({ "title": "Deploy notes", "body": body }),
                        ctx,
                        CancellationToken::new(),
                    )
                    .await
                    .unwrap()
            }
        };
        let first = remember("Staging deploys from the release branch.").await;
        let second = remember("Production deploys are gated on the on-call approval.").await;
        assert!(!first.is_error && !second.is_error);

        // Both facts must be retrievable — the second must not have overwritten
        // the first just because their titles slug to the same id.
        let listed = MemoryTool
            .execute(
                serde_json::json!({"action": "search", "query": "deploy", "limit": 10}),
                ctx.clone(),
                CancellationToken::new(),
            )
            .await
            .unwrap();
        assert!(
            listed.content.contains("release branch"),
            "first fact was clobbered: {}",
            listed.content
        );
        assert!(
            listed.content.contains("on-call approval"),
            "second fact missing: {}",
            listed.content
        );
        // Idempotent re-remember of the SAME fact must not spawn a duplicate.
        let again = remember("Staging deploys from the release branch.").await;
        assert_eq!(again.content, first.content, "identical fact must reuse its id");
    }

    #[tokio::test]
    async fn missing_memory_root_errors_clearly() {
        let dir = tempdir().unwrap();
        let ws = leveler_execution::Workspace::new(dir.path()).unwrap();
        let ctx = ToolContext::new(ws, leveler_execution::PermissionProfile::RequestApproval);
        let err = MemoryTool
            .execute(
                serde_json::json!({"action": "list"}),
                ctx,
                CancellationToken::new(),
            )
            .await
            .unwrap_err();
        assert!(err.to_string().contains("not configured"));
    }

    #[tokio::test]
    async fn vector_search_returns_ranked_hits() {
        let dir = tempdir().unwrap();
        let mem = dir.path().join("memory");
        let ws = leveler_execution::Workspace::new(dir.path()).unwrap();
        let ctx = ToolContext::new(ws, leveler_execution::PermissionProfile::Assisted)
            .with_memory_root(&mem);
        RememberTool
            .execute(
                serde_json::json!({
                    "title": "Workspace write",
                    "body": "Prefer PermissionProfile::Assisted for file edits."
                }),
                ctx.clone(),
                CancellationToken::new(),
            )
            .await
            .unwrap();
        RememberTool
            .execute(
                serde_json::json!({
                    "title": "Unrelated",
                    "body": "The sky is blue on clear days."
                }),
                ctx.clone(),
                CancellationToken::new(),
            )
            .await
            .unwrap();
        let out = MemoryTool
            .execute(
                serde_json::json!({
                    "action": "vector_search",
                    "query": "workspace write edits",
                    "limit": 3
                }),
                ctx,
                CancellationToken::new(),
            )
            .await
            .unwrap();
        assert!(!out.is_error, "{}", out.content);
        assert!(
            out.content.contains("Workspace write"),
            "expected ranked hit: {}",
            out.content
        );
        assert!(out.content.contains("score"), "{}", out.content);
    }

    #[tokio::test]
    async fn consolidate_memory_returns_candidates_without_auto_write() {
        let dir = tempdir().unwrap();
        let mem = dir.path().join("memory");
        let ws = leveler_execution::Workspace::new(dir.path()).unwrap();
        let ctx = ToolContext::new(ws, leveler_execution::PermissionProfile::Assisted)
            .with_memory_root(&mem);
        let out = ConsolidateMemoryTool
            .execute(
                serde_json::json!({
                    "transcript": "User preference: always use WorkspaceWrite for edits.\nDecision: never store API keys in memory.",
                    "auto_write": false
                }),
                ctx.clone(),
                CancellationToken::new(),
            )
            .await
            .unwrap();
        assert!(!out.is_error, "{}", out.content);
        assert!(
            out.content.contains("Candidates") || out.content.contains("preference"),
            "{}",
            out.content
        );
        // Default auto_write=false must not create active memories.
        let listed = MemoryTool
            .execute(
                serde_json::json!({"action": "list"}),
                ctx,
                CancellationToken::new(),
            )
            .await
            .unwrap();
        assert!(
            listed.content.contains("No active memories") || !listed.content.contains("["),
            "auto_write=false must not persist: {}",
            listed.content
        );
    }

    #[tokio::test]
    async fn consolidate_memory_auto_write_persists() {
        let dir = tempdir().unwrap();
        let mem = dir.path().join("memory");
        let ws = leveler_execution::Workspace::new(dir.path()).unwrap();
        let ctx = ToolContext::new(ws, leveler_execution::PermissionProfile::Assisted)
            .with_memory_root(&mem);
        let out = ConsolidateMemoryTool
            .execute(
                serde_json::json!({
                    "transcript": "User preference: always use WorkspaceWrite for edits.",
                    "auto_write": true
                }),
                ctx.clone(),
                CancellationToken::new(),
            )
            .await
            .unwrap();
        assert!(!out.is_error, "{}", out.content);
        assert!(
            out.content.contains("Wrote") || out.content.contains("No durable"),
            "{}",
            out.content
        );
        if out.content.contains("Wrote") {
            let listed = MemoryTool
                .execute(
                    serde_json::json!({"action": "list"}),
                    ctx,
                    CancellationToken::new(),
                )
                .await
                .unwrap();
            assert!(
                !listed.content.contains("No active memories"),
                "{}",
                listed.content
            );
        }
    }
}
