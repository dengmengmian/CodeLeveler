//! Memory tools: `memory` (search/list/read), `remember`, `forget`.
//!
//! Each tool is CONSTRUCTED with the store root (the app passes
//! `Layout::memory_dir`); it used to read one out of `ToolContext.services`,
//! where every unrelated tool could reach it too. `None` means memory is not
//! configured for this run, and the tool says so rather than inventing a
//! location. Writes require human approval (K36).

use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use schemars::JsonSchema;
use serde::Deserialize;
use tokio_util::sync::CancellationToken;

use leveler_execution::RiskLevel;
use leveler_memory::MemoryStore;

use crate::tool::{Tool, ToolContext, ToolError, ToolOutput};

/// The memory-store root a memory tool is built with, shared by the three.
#[derive(Clone)]
pub struct MemoryRoot(Option<Arc<PathBuf>>);

impl MemoryRoot {
    pub fn new(root: Option<PathBuf>) -> Self {
        Self(root.map(Arc::new))
    }

    fn open(&self) -> Result<MemoryStore, ToolError> {
        let root = self.0.as_ref().ok_or_else(|| {
            ToolError::Io(
                "memory store is not configured for this session (app must set Layout::memory_dir)"
                    .to_string(),
            )
        })?;
        MemoryStore::open(root.as_path()).map_err(|e| ToolError::Io(e.to_string()))
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

pub struct MemoryTool {
    root: MemoryRoot,
}

impl MemoryTool {
    pub fn new(root: MemoryRoot) -> Self {
        Self { root }
    }
}

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
        _context: ToolContext,
        _cancellation: CancellationToken,
    ) -> Result<ToolOutput, ToolError> {
        let args: MemoryArgs = super::parse_input(self.name(), input)?;
        let store = self.root.open()?;
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
    /// How this memory should reach future turns. Required of the model
    /// rather than defaulted silently: `preference` is paid for on EVERY
    /// later turn, so it must be a choice, not an accident.
    kind: ToolMemoryKind,
    #[serde(default)]
    tags: Vec<String>,
}

/// The kinds a proposal may claim.
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
enum ToolMemoryKind {
    /// Injected into every future turn. Only for lasting how-to-work rules.
    Preference,
    /// A decision worth keeping; found by relevance or by title.
    Decision,
    /// Anything else worth keeping; same reach as a decision.
    Note,
}

impl From<ToolMemoryKind> for leveler_memory::MemoryKind {
    fn from(value: ToolMemoryKind) -> Self {
        match value {
            ToolMemoryKind::Preference => Self::Preference,
            ToolMemoryKind::Decision => Self::Decision,
            ToolMemoryKind::Note => Self::Note,
        }
    }
}

pub struct RememberTool {
    root: MemoryRoot,
}

impl RememberTool {
    pub fn new(root: MemoryRoot) -> Self {
        Self { root }
    }
}

#[async_trait]
impl Tool for RememberTool {
    fn name(&self) -> &'static str {
        "remember"
    }

    fn description(&self) -> &'static str {
        "Propose a durable project memory (title + body + kind). Use it when \
         the user states a lasting preference, a decision or project \
         convention, or a non-obvious fact worth carrying into later sessions. \
         This is a PROPOSAL: a reachable human must approve it in every \
         permission profile, full access included, and the approval prompt IS \
         the user's consent — so propose rather than asking in prose. If the \
         user already saved it themselves with `/remember`, do not propose it \
         again. Pick `kind` deliberately: `preference` is injected into every \
         future turn, while `decision` and `note` are retrieved when relevant, \
         which is the right choice for most facts. This does NOT overwrite: \
         re-proposing an existing title with different content stores a second \
         entry, so correct a superseded memory with `forget` on the old id \
         first. Not for one-off trivia, secrets, raw transcripts, or anything \
         already in the code, git history, lockfiles, or AGENTS.md."
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
        _context: ToolContext,
        _cancellation: CancellationToken,
    ) -> Result<ToolOutput, ToolError> {
        let args: RememberArgs = super::parse_input(self.name(), input)?;
        if args.title.trim().is_empty() || args.body.trim().is_empty() {
            return Ok(ToolOutput::error("title and body are required"));
        }
        let store = self.root.open()?;
        // One activation entry point for every writer, so overwrite, dedup,
        // secret refusal and pending cleanup cannot differ by caller.
        let saved = store
            .activate(&args.title, &args.body, args.kind.into(), args.tags)
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

pub struct ForgetTool {
    root: MemoryRoot,
}

impl ForgetTool {
    pub fn new(root: MemoryRoot) -> Self {
        Self { root }
    }
}

#[async_trait]
impl Tool for ForgetTool {
    fn name(&self) -> &'static str {
        "forget"
    }

    fn description(&self) -> &'static str {
        "Archive a durable memory by id (soft-delete; retained for audit). Use it \
         when a stored memory is contradicted by the current code — the file, \
         function, or flag it names is gone, or it was never right — and as the \
         first half of correcting a superseded one (forget the old id, then \
         `remember` the corrected version). Requires user approval."
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
        _context: ToolContext,
        _cancellation: CancellationToken,
    ) -> Result<ToolOutput, ToolError> {
        let args: ForgetArgs = super::parse_input(self.name(), input)?;
        let store = self.root.open()?;
        let entry = store
            .forget(&args.id)
            .map_err(|e| ToolError::Io(e.to_string()))?;
        Ok(ToolOutput::ok(format!(
            "Archived memory [{}]: {}",
            entry.id, entry.title
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn ctx_in(root: &std::path::Path) -> ToolContext {
        let ws = leveler_execution::Workspace::new(root).unwrap();
        ToolContext::new(ws, leveler_execution::PermissionProfile::Assisted)
    }

    /// The root a memory tool writes to is the one it was CONSTRUCTED with —
    /// never an ambient environment variable, and never a location invented at
    /// call time.
    #[tokio::test]
    async fn writes_to_the_root_it_was_constructed_with() {
        let dir = tempdir().unwrap();
        let mem = dir.path().join("memory");
        let root = MemoryRoot::new(Some(mem.clone()));
        let ctx = ctx_in(dir.path());
        let out = RememberTool::new(root.clone())
            .execute(
                serde_json::json!({"title": "Prefer workspace-write", "body": "Use PermissionProfile::Assisted for edits.", "kind": "note"}),
                ctx.clone(),
                CancellationToken::new(),
            )
            .await
            .unwrap();
        assert!(!out.is_error, "{}", out.content);
        assert!(mem.join("active").exists());
        let listed = MemoryTool::new(root)
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
        let root = MemoryRoot::new(Some(mem.clone()));
        let ctx = ctx_in(dir.path());
        let remember = |body: &'static str| {
            let ctx = ctx.clone();
            let root = root.clone();
            async move {
                RememberTool::new(root)
                    .execute(
                        serde_json::json!({"title": "Deploy notes", "body": body, "kind": "note"}),
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
        let listed = MemoryTool::new(MemoryRoot::new(Some(mem.clone())))
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
        assert_eq!(
            again.content, first.content,
            "identical fact must reuse its id"
        );
    }

    /// A run with memory unconfigured says so. Nothing invents a store
    /// location, and nothing silently succeeds against one.
    #[tokio::test]
    async fn an_unconfigured_memory_root_errors_clearly() {
        let dir = tempdir().unwrap();
        let ctx = ctx_in(dir.path());
        let err = MemoryTool::new(MemoryRoot::new(None))
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
        let root = MemoryRoot::new(Some(mem.clone()));
        let ctx = ctx_in(dir.path());
        RememberTool::new(root.clone())
            .execute(
                serde_json::json!({"title": "Workspace write", "body": "Prefer PermissionProfile::Assisted for file edits.", "kind": "note"}),
                ctx.clone(),
                CancellationToken::new(),
            )
            .await
            .unwrap();
        RememberTool::new(root.clone())
            .execute(
                serde_json::json!({"title": "Unrelated", "body": "The sky is blue on clear days.", "kind": "note"}),
                ctx.clone(),
                CancellationToken::new(),
            )
            .await
            .unwrap();
        let out = MemoryTool::new(root)
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
}
