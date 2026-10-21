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
use leveler_memory::{MemoryStore, is_sensitive};

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
                // `effective_active` is the one current-truth read: it drops
                // expired entries and collapses crash residue by identity, so
                // a stale copy is never presented to the model as a memory.
                let entries = store
                    .effective_active()
                    .map_err(|e| ToolError::Io(e.to_string()))?
                    .into_iter()
                    .filter(|entry| !is_sensitive(entry))
                    .collect::<Vec<_>>();
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
                if is_sensitive(&e) {
                    return Ok(ToolOutput::error("memory not found"));
                }
                // An id can still exist on disk as crash residue for an
                // identity whose terminal value is another entry. Reading it
                // as current truth would bypass recall's deduplication.
                let current = store
                    .effective_active()
                    .map_err(|err| ToolError::Io(err.to_string()))?
                    .into_iter()
                    .any(|entry| entry.id == e.id);
                if !current {
                    return Ok(ToolOutput::error(
                        "memory is historical or superseded; it is not current truth",
                    ));
                }
                Ok(ToolOutput::ok(format!(
                    "# {}\n\n{}\n\n(tags: {})",
                    e.title,
                    e.body,
                    e.tags.join(", ")
                )))
            }
            "search" | "vector_search" => {
                let q = args.query.unwrap_or_default();
                // Search the full active set before filtering. Asking the
                // store for only the user's limit could let sensitive hits
                // occupy that window and hide safe matches behind them.
                let active_count = store
                    .list_active()
                    .map_err(|e| ToolError::Io(e.to_string()))?
                    .len();
                let search_limit = active_count.max(args.limit.max(1));
                let mut hits = if args.action == "vector_search" {
                    store
                        .vector_search(&q, search_limit)
                        .map_err(|e| ToolError::Io(e.to_string()))?
                } else {
                    store
                        .search(&q, search_limit)
                        .map_err(|e| ToolError::Io(e.to_string()))?
                };
                hits.retain(|(entry, _)| !is_sensitive(entry));
                hits.truncate(args.limit.max(1));
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
    /// The durable subject this fact is about, e.g. `project.default_model`.
    /// Required: without a stable identity the lifecycle cannot tell an update
    /// from a new fact, so the model must state what the fact is about.
    subject: String,
    /// How this memory should reach future turns. Required of the model
    /// rather than defaulted silently: `preference` is paid for on EVERY
    /// later turn, so it must be a choice, not an accident.
    kind: ToolMemoryKind,
    #[serde(default)]
    tags: Vec<String>,
    /// New evidence for an update of the model's own earlier memory. It is
    /// persisted with the entry; it never turns model inference into a user
    /// fact.
    #[serde(default)]
    evidence: Option<String>,
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
        "Propose a durable project memory: `title`, `body`, `subject`, and \
         `kind` (`preference`, `decision`, or `note`). `subject` is the stable \
         fact identity (e.g. `project.default_model`): a later value for the \
         same subject updates the current memory instead of adding a second \
         entry. Auto and Restricted require a person to approve the proposal \
         before it becomes active; Full permits the write without a separate \
         permission prompt. With nobody to ask, the proposal may stay pending. \
         The entry records model-inference authority: approval does not claim \
         the user stated it, and it never replaces a fact the user wrote \
         directly — a conflict waits for a person. To update the model's own \
         earlier memory, pass new `evidence`; otherwise it also waits for a \
         person. `preference` is injected into later turns. `decision` and \
         `note` are retrieved by relevance or title. `forget` archives an id. \
         Secrets are not a valid body."
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
        // A model proposal is not a user statement. The candidate carries
        // model-inference authority and runs the SAME lifecycle decision as
        // every other write: identity match, merge/replace its own earlier
        // value, or wait for a person. Approval to execute the tool never
        // upgrades the fact's authority.
        let candidate = leveler_memory::agent_candidate(
            &args.title,
            &args.body,
            args.kind.into(),
            args.tags,
            &args.subject,
            args.evidence,
        )
        .map_err(|e| ToolError::InvalidArguments {
            tool: self.name().into(),
            message: e.to_string(),
        })?;
        let outcome = store
            .admit_agent_candidate(candidate)
            .map_err(|e| ToolError::Io(e.to_string()))?;
        let message = match outcome {
            leveler_memory::AgentAdmitOutcome::Applied(outcome) => {
                let entry = outcome.entry.ok_or_else(|| {
                    ToolError::Io("memory lifecycle applied no entry".to_string())
                })?;
                match outcome.operation {
                    leveler_memory::AppliedOperation::Created => {
                        format!("Remembered [{}]: {}", entry.id, entry.title)
                    }
                    leveler_memory::AppliedOperation::Superseded => {
                        let old = outcome
                            .superseded
                            .first()
                            .map(|old| old.id.as_str())
                            .unwrap_or("previous value");
                        format!(
                            "Updated [{}]: {} (superseded [{}])",
                            entry.id, entry.title, old
                        )
                    }
                    leveler_memory::AppliedOperation::Merged => {
                        format!("Merged into [{}]: {}", entry.id, entry.title)
                    }
                    leveler_memory::AppliedOperation::Skipped
                    | leveler_memory::AppliedOperation::NeedsApproval => {
                        return Err(ToolError::Io(
                            "memory lifecycle returned an inconsistent outcome".to_string(),
                        ));
                    }
                }
            }
            leveler_memory::AgentAdmitOutcome::Queued { candidate, reason } => format!(
                "Queued for approval [{}]: {}. A person decides; the current memory is unchanged.",
                candidate.id, reason
            ),
            leveler_memory::AgentAdmitOutcome::NotStored { reason } => {
                format!("Not stored: {reason}")
            }
        };
        Ok(ToolOutput::ok(message))
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
        "Archive a durable memory by id. The entry is retained for audit and \
         is no longer active. Requires user approval. Archiving does not write \
         a replacement; a replacement is a separate `remember` proposal."
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
                serde_json::json!({"title": "Prefer workspace-write", "body": "Use PermissionProfile::Assisted for edits.", "kind": "note", "subject": "editing.permission_profile"}),
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
        let remember = |body: &'static str, subject: &'static str| {
            let ctx = ctx.clone();
            let root = root.clone();
            async move {
                RememberTool::new(root)
                    .execute(
                        serde_json::json!({
                            "title": "Deploy notes",
                            "body": body,
                            "kind": "note",
                            "subject": subject
                        }),
                        ctx,
                        CancellationToken::new(),
                    )
                    .await
                    .unwrap()
            }
        };
        let first = remember("Staging deploys from the release branch.", "deploy.staging").await;
        let second = remember(
            "Production deploys are gated on the on-call approval.",
            "deploy.production",
        )
        .await;
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
        let again = remember("Staging deploys from the release branch.", "deploy.staging").await;
        assert!(
            again.content.contains("identical") || again.content.contains("already states this"),
            "identical fact must be recognized as current: {}",
            again.content
        );
        let store = MemoryStore::open(&mem).unwrap();
        assert_eq!(store.effective_active().unwrap().len(), 2);
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
                serde_json::json!({"title": "Workspace write", "body": "Prefer PermissionProfile::Assisted for file edits.", "kind": "note", "subject": "editing.permission_profile"}),
                ctx.clone(),
                CancellationToken::new(),
            )
            .await
            .unwrap();
        RememberTool::new(root.clone())
            .execute(
                serde_json::json!({"title": "Unrelated", "body": "The sky is blue on clear days.", "kind": "note", "subject": "weather.sky"}),
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

    /// The model's `remember` call is a model finding, even when a human
    /// approved executing it. It must not claim the user said it, and it must
    /// never replace a fact the user wrote directly.
    #[tokio::test]
    async fn agent_remember_is_model_authority_and_never_replaces_a_user_fact() {
        let dir = tempdir().unwrap();
        let mem = dir.path().join("memory");
        let root = MemoryRoot::new(Some(mem.clone()));
        let ctx = ctx_in(dir.path());
        let store = MemoryStore::open(&mem).unwrap();
        let user = store
            .activate(
                "Deploy notes",
                "User chose the release branch.",
                leveler_memory::MemoryKind::Note,
                vec![],
            )
            .unwrap();

        let out = RememberTool::new(root.clone())
            .execute(
                serde_json::json!({
                    "title": "Deploy notes",
                    "body": "The model thinks production is gated on on-call approval.",
                    "kind": "note",
                    "subject": "deploy.production"
                }),
                ctx.clone(),
                CancellationToken::new(),
            )
            .await
            .unwrap();
        assert!(!out.is_error, "{}", out.content);

        let active = store.list_active().unwrap();
        assert_eq!(active.len(), 2, "the model adds a second entry: {active:?}");
        let user_entry = active.iter().find(|entry| entry.id == user.id).unwrap();
        assert_eq!(
            user_entry.body, "User chose the release branch.",
            "the user's entry must be untouched"
        );
        assert_eq!(
            user_entry.provenance.as_ref().unwrap().authority,
            leveler_memory::MemoryAuthority::ExplicitUser
        );
        let agent_entry = active
            .iter()
            .find(|entry| entry.id != user.id)
            .expect("agent entry");
        assert_eq!(
            agent_entry.provenance.as_ref().unwrap().authority,
            leveler_memory::MemoryAuthority::ModelInference,
            "approval to execute is not a claim that the user asserted the fact"
        );
        assert_eq!(
            agent_entry.provenance.as_ref().unwrap().source,
            "agent_remember_tool"
        );
    }

    /// The same semantic subject is one fact: a later value updates the
    /// current entry instead of accumulating a second active copy.
    #[tokio::test]
    async fn agent_remember_with_the_same_subject_updates_instead_of_duplicating() {
        let dir = tempdir().unwrap();
        let mem = dir.path().join("memory");
        let root = MemoryRoot::new(Some(mem.clone()));
        let ctx = ctx_in(dir.path());
        let remember = |body: &'static str, evidence: Option<&'static str>| {
            let ctx = ctx.clone();
            let root = root.clone();
            async move {
                let mut args = serde_json::json!({
                    "title": "项目默认模型",
                    "body": body,
                    "kind": "note",
                    "subject": "project.default_model"
                });
                if let Some(evidence) = evidence {
                    args["evidence"] = serde_json::Value::String(evidence.to_string());
                }
                RememberTool::new(root)
                    .execute(args, ctx, CancellationToken::new())
                    .await
                    .unwrap()
            }
        };

        let first = remember("项目默认模型是 Pro", None).await;
        assert!(!first.is_error, "{}", first.content);
        let second = remember(
            "项目默认模型是 Flash",
            Some("the user restated the model in this turn"),
        )
        .await;
        assert!(!second.is_error, "{}", second.content);

        let store = MemoryStore::open(&mem).unwrap();
        let active = store.effective_active().unwrap();
        assert_eq!(active.len(), 1, "one current truth: {active:?}");
        assert!(active[0].body.contains("Flash"));
        let history = store.list_archived().unwrap();
        assert_eq!(history.len(), 1, "the old value is history");
        assert!(history[0].body.contains("Pro"));
    }

    /// Approval to execute a tool is not a claim that the model's fact
    /// outranks the user's. A conflicting agent value waits for a person.
    #[tokio::test]
    async fn agent_remember_conflicting_with_a_user_fact_waits_for_approval() {
        let dir = tempdir().unwrap();
        let mem = dir.path().join("memory");
        let root = MemoryRoot::new(Some(mem.clone()));
        let ctx = ctx_in(dir.path());
        let store = MemoryStore::open(&mem).unwrap();
        let user = leveler_memory::parse_durable_fact("以后本项目默认模型固定为 Pro").unwrap();
        store.commit_candidate(&user).unwrap();

        let out = RememberTool::new(root)
            .execute(
                serde_json::json!({
                    "title": "项目默认模型",
                    "body": "项目默认模型是 Flash",
                    "kind": "note",
                    "subject": "project.default_model"
                }),
                ctx,
                CancellationToken::new(),
            )
            .await
            .unwrap();

        assert!(!out.is_error, "{}", out.content);
        assert!(
            out.content.contains("approval") || out.content.contains("等待"),
            "the model must be told a person decides: {}",
            out.content
        );
        let active = store.effective_active().unwrap();
        assert_eq!(active.len(), 1);
        assert!(active[0].body.contains("Pro"), "{active:?}");
        assert_eq!(store.list_archived().unwrap().len(), 0);
        assert_eq!(store.list_pending().unwrap().len(), 1, "conflict is kept");
    }

    /// Every model-facing read presents current truth. Crash residue can leave
    /// the replacement and the old copy active together; neither `list` nor
    /// `read` may present the loser as a memory.
    #[tokio::test]
    async fn reads_never_present_crash_residue_as_current_truth() {
        let dir = tempdir().unwrap();
        let mem = dir.path().join("memory");
        let root = MemoryRoot::new(Some(mem.clone()));
        let ctx = ctx_in(dir.path());
        let store = MemoryStore::open(&mem).unwrap();

        let mut old = leveler_memory::new_entry("Old value", "默认模型：Pro", vec![]);
        old.id = "old-pro".into();
        old.key = Some("k-defaultmodel".into());
        old.updated_at = "2026-01-01T00:00:00Z".into();
        old.created_at = old.updated_at.clone();
        store.remember(old).unwrap();

        let mut new = leveler_memory::new_entry("New value", "默认模型：Flash", vec![]);
        new.id = "new-flash".into();
        new.key = Some("k-defaultmodel".into());
        new.updated_at = "2026-01-01T00:00:00Z".into();
        new.created_at = new.updated_at.clone();
        new.supersedes = Some("old-pro".into());
        store.remember(new).unwrap();

        let listed = MemoryTool::new(root.clone())
            .execute(
                serde_json::json!({"action": "list"}),
                ctx.clone(),
                CancellationToken::new(),
            )
            .await
            .unwrap();
        assert!(listed.content.contains("New value"), "{}", listed.content);
        assert!(!listed.content.contains("Old value"), "{}", listed.content);

        let read = MemoryTool::new(root)
            .execute(
                serde_json::json!({"action": "read", "id": "old-pro"}),
                ctx,
                CancellationToken::new(),
            )
            .await
            .unwrap();
        assert!(read.is_error, "stale residue is not current truth");
    }

    #[tokio::test]
    async fn remember_requires_a_subject_and_preserves_the_declared_kind() {
        let dir = tempdir().unwrap();
        let mem = dir.path().join("memory");
        let root = MemoryRoot::new(Some(mem.clone()));
        let ctx = ctx_in(dir.path());

        let missing = RememberTool::new(root.clone())
            .execute(
                serde_json::json!({
                    "title": "No subject",
                    "body": "This fact has no identity.",
                    "kind": "note"
                }),
                ctx.clone(),
                CancellationToken::new(),
            )
            .await;
        assert!(missing.is_err(), "a subject is required");

        let out = RememberTool::new(root)
            .execute(
                serde_json::json!({
                    "title": "Release rule",
                    "body": "Tag only after the full test suite passes.",
                    "kind": "decision",
                    "subject": "release.tagging"
                }),
                ctx,
                CancellationToken::new(),
            )
            .await
            .unwrap();
        assert!(!out.is_error, "{}", out.content);
        let store = MemoryStore::open(&mem).unwrap();
        let active = store.effective_active().unwrap();
        assert_eq!(active[0].kind.as_deref(), Some("decision"));
    }

    /// Stores created before credential validation may already contain a key.
    /// The model-facing tool must not expose it through any read action, even
    /// though the file remains available for the user's own cleanup/audit.
    #[tokio::test]
    async fn historical_sensitive_memory_is_hidden_from_every_read_action() {
        let dir = tempdir().unwrap();
        let mem = dir.path().join("memory");
        let root = MemoryRoot::new(Some(mem.clone()));
        let ctx = ctx_in(dir.path());
        let store = MemoryStore::open(&mem).unwrap();
        let safe = leveler_memory::new_entry(
            "Deploy preference",
            "Use foreground service startup.",
            vec![],
        );
        store.remember(safe).unwrap();

        let secret = leveler_memory::new_entry(
            "Legacy API key",
            "API_KEY=demo-provider-value-1234567890",
            vec![],
        );
        let secret_id = secret.id.clone();
        std::fs::write(
            mem.join("active").join(format!("{secret_id}.json")),
            serde_json::to_vec_pretty(&secret).unwrap(),
        )
        .unwrap();

        for input in [
            serde_json::json!({"action": "list"}),
            serde_json::json!({"action": "search", "query": "API key provider", "limit": 10}),
            serde_json::json!({"action": "vector_search", "query": "API key provider", "limit": 10}),
        ] {
            let out = MemoryTool::new(root.clone())
                .execute(input, ctx.clone(), CancellationToken::new())
                .await
                .unwrap();
            assert!(!out.content.contains(&secret_id), "{}", out.content);
            assert!(!out.content.contains("demo-provider"), "{}", out.content);
        }

        let read = MemoryTool::new(root)
            .execute(
                serde_json::json!({"action": "read", "id": secret_id}),
                ctx,
                CancellationToken::new(),
            )
            .await
            .unwrap();
        assert!(read.is_error, "sensitive direct read must be refused");
        assert!(!read.content.contains("demo-provider"), "{}", read.content);
    }
}
