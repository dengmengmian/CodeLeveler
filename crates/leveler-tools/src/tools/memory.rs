//! Memory tools: `memory` (list/read/search/update/deprecate/pin/unpin),
//! `remember`, `forget`.
//!
//! Each tool is CONSTRUCTED with the one [`MemoryService`]; it used to read a
//! store root out of `ToolContext.services`, where every unrelated tool could
//! reach it too. `None` means memory is not configured for this run, and the
//! tool says so rather than inventing a location.
//!
//! Authority comes from the execution context, never from the tool payload.
//! A write carries the host-stamped session/turn/call identity, so the store
//! records it as a model finding; there is no argument a model can pass to
//! become a user decision. Reads need no identity because they change nothing.

use std::sync::Arc;

use async_trait::async_trait;
use schemars::JsonSchema;
use serde::Deserialize;
use tokio_util::sync::CancellationToken;

use leveler_execution::RiskLevel;
use leveler_memory::{
    MemoryService, MemoryView, RememberRequest, ServiceError, ServiceScope, UpdateRequest,
    WriteOutcome,
};

use crate::tool::{Tool, ToolContext, ToolError, ToolOutput};

/// The memory service and lane policy a memory tool is built with.
#[derive(Clone)]
pub struct MemoryRoot {
    service: Option<Arc<MemoryService>>,
    lanes: leveler_memory::MemoryLanes,
}

impl MemoryRoot {
    /// A legacy JSON store rooted at `root`. Kept for callers that only know a
    /// store directory; the app hands over the configured service instead.
    pub fn new(root: Option<std::path::PathBuf>) -> Self {
        Self {
            service: root.map(|root| Arc::new(MemoryService::legacy(root))),
            lanes: leveler_memory::MemoryLanes::ALL,
        }
    }

    pub fn from_service(service: Option<Arc<MemoryService>>) -> Self {
        Self {
            service,
            lanes: leveler_memory::MemoryLanes::ALL,
        }
    }

    /// Restrict every read and write through this handle to the effective
    /// lanes. The registry uses one handle for all three tools.
    pub fn with_lanes(mut self, lanes: leveler_memory::MemoryLanes) -> Self {
        self.lanes = lanes;
        self
    }

    fn lanes(&self) -> leveler_memory::MemoryLanes {
        self.lanes
    }

    fn service(&self) -> Result<&MemoryService, ToolError> {
        self.service.as_deref().ok_or_else(|| {
            ToolError::Io(
                "memory store is not configured for this session (app must set Layout::memory_dir)"
                    .to_string(),
            )
        })
    }
}

fn parse_scope(scope: Option<&str>) -> Result<ServiceScope, ToolError> {
    match scope.unwrap_or("project") {
        "project" => Ok(ServiceScope::Project),
        "local" => Ok(ServiceScope::Local),
        "global" => Ok(ServiceScope::Global),
        other => Err(ToolError::InvalidArguments {
            tool: "memory".into(),
            message: format!("unknown scope `{other}`; use project|local|global"),
        }),
    }
}

/// The model actor for one dispatched call. All three identities come from the
/// runtime; a call without them cannot claim to be a model write.
fn model_actor(context: &ToolContext) -> Result<leveler_memory::v2::Actor, ToolError> {
    let session = context.session_scope.as_deref().ok_or_else(|| {
        ToolError::Io("memory write has no session identity in its execution context".into())
    })?;
    let turn = context.turn_scope.as_deref().ok_or_else(|| {
        ToolError::Io("memory write has no turn identity in its execution context".into())
    })?;
    let tool_call = context.tool_call_scope.as_deref().ok_or_else(|| {
        ToolError::Io("memory write has no tool-call identity in its execution context".into())
    })?;
    Ok(leveler_memory::v2::Actor::Model {
        session: leveler_core::SessionId::new(session),
        turn: leveler_core::TurnId::new(turn),
        tool_call: leveler_core::ToolCallId::new(tool_call),
    })
}

fn service_error(tool: &str, error: ServiceError) -> ToolError {
    match error {
        ServiceError::Invalid(message) => ToolError::InvalidArguments {
            tool: tool.into(),
            message,
        },
        other => ToolError::Io(other.to_string()),
    }
}

fn outcome_message(outcome: &WriteOutcome) -> String {
    match outcome {
        WriteOutcome::Applied {
            id,
            title,
            operation,
            superseded,
            replayed,
            possible_duplicate,
            ..
        } => {
            let mut line = match operation.as_str() {
                "created" => format!("Remembered [{id}]: {title}"),
                "superseded" => format!("Updated [{id}]: {title}"),
                "merged" => format!("Merged into [{id}]: {title}"),
                other => format!("Applied {other} [{id}]: {title}"),
            };
            if !superseded.is_empty() {
                line.push_str(&format!(" (superseded [{}])", superseded.join(", ")));
            }
            if *replayed {
                line.push_str(" (replayed; already applied)");
            }
            if !possible_duplicate.is_empty() {
                let ids = possible_duplicate
                    .iter()
                    .map(String::as_str)
                    .collect::<Vec<_>>()
                    .join(", ");
                line.push_str(&format!(" (possible duplicate: [{ids}])"));
            }
            line
        }
        WriteOutcome::Pending {
            id,
            title,
            reason,
        } => format!(
            "Queued for approval [{id}]: {title}. A person decides; the current memory is \
             unchanged. ({reason})"
        ),
        WriteOutcome::Skipped { existing, reason } => {
            if existing.is_empty() {
                format!("Not stored: {reason}")
            } else {
                format!("Not stored: {reason} (current memory [{existing}])")
            }
        }
        WriteOutcome::Conflict {
            target,
            current_revision,
            expected_revision,
        } => format!(
            "Conflict on [{target}]: current revision {current_revision}, expected \
             {expected_revision}. Re-read it and retry."
        ),
        WriteOutcome::Rejected { reason } => format!("Rejected: {reason}"),
        WriteOutcome::NotStored { reason } => format!("Not stored: {reason}"),
        WriteOutcome::Forgotten { id, title } => format!("Archived memory [{id}]: {title}"),
    }
}

fn render_view(view: &MemoryView, with_body: bool, scope: ServiceScope) -> String {
    if !with_body {
        return format!("- [{}] {}", view.id, view.title);
    }
    let scope = match scope {
        ServiceScope::Project => "project",
        ServiceScope::Local => "local",
        ServiceScope::Global => "global",
    };
    // `read` is the handle a caller needs before an update/deprecate: without
    // the identity, status and scope on the returned record, the revision
    // alone cannot say what was read or whether it is still current truth.
    let mut body = format!(
        "# {}\n\n(id: {}; status: {}; scope: {}; authority: {})\n\n{}",
        view.title, view.id, view.status, scope, view.authority, view.body
    );
    if let Some(experience) = view.experience.as_ref() {
        body.push_str(&format!(
            "\n\n(experience·{}; applies when: {}",
            experience.standing, experience.applies_when
        ));
        if let Some(invalid_if) = experience.invalid_if.as_deref() {
            body.push_str(&format!("; invalid if: {invalid_if}"));
        }
        body.push(')');
    }
    if !view.tags.is_empty() {
        body.push_str(&format!("\n\n(tags: {})", view.tags.join(", ")));
    }
    if !view.current {
        body.push_str("\n\n(not current)");
    }
    if let Some(revision) = view.revision {
        body.push_str(&format!("\n\n(revision {revision})"));
    }
    body
}

#[derive(Debug, Deserialize, JsonSchema)]
struct MemoryArgs {
    /// Action: search | vector_search | list | read | update | deprecate | pin | unpin
    action: String,
    /// Storage scope: project (default) | local | global.
    #[serde(default)]
    scope: Option<String>,
    /// Search query (action=search/vector_search) or entry id (action=read).
    #[serde(default)]
    query: Option<String>,
    #[serde(default)]
    id: Option<String>,
    /// Required for update/deprecate: the revision the caller read.
    #[serde(default)]
    expected_revision: Option<u64>,
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    body: Option<String>,
    #[serde(default)]
    value: Option<String>,
    #[serde(default)]
    tags: Option<Vec<String>>,
    #[serde(default)]
    expires_at: Option<String>,
    #[serde(default)]
    reason: Option<String>,
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
        "Read and maintain durable memories. action=list|read|search|update|deprecate|pin|unpin. \
         Bodies are not in the system prompt — retrieve them here. Search and vector_search are \
         unavailable on the Markdown backend until Stage 3; the tool says so instead of returning \
         an empty result. update and deprecate require the revision the caller read. A write to a \
         record the user owns is queued for the user; the model cannot accept or reject pending \
         memory."
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
        let service = self.root.service()?;
        let lanes = self.root.lanes();
        let scope = parse_scope(args.scope.as_deref())?;
        let read_actor = model_actor(&context).unwrap_or(leveler_memory::v2::Actor::External);
        match args.action.as_str() {
            "list" => {
                let views = service
                    .list(&read_actor, scope, lanes, Some(args.limit.max(1)))
                    .map_err(|error| service_error(self.name(), error))?;
                if views.is_empty() {
                    return Ok(ToolOutput::ok("No active memories.".to_string()));
                }
                let mut body = String::new();
                for view in views {
                    body.push_str(&render_view(&view, false, scope));
                    body.push('\n');
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
                match service.read(&read_actor, scope, &id) {
                    Ok(view) => {
                        if lanes.allows_experience(view.is_experience()) {
                            Ok(ToolOutput::ok(render_view(&view, true, scope)))
                        } else {
                            Ok(ToolOutput::error("memory not found"))
                        }
                    }
                    Err(ServiceError::NotFound(_)) => Ok(ToolOutput::error("memory not found")),
                    Err(ServiceError::NotCurrent(_)) => Ok(ToolOutput::error(
                        "memory is historical or superseded; it is not current truth",
                    )),
                    Err(error) => Err(service_error(self.name(), error)),
                }
            }
            "search" | "vector_search" => {
                let q = args.query.unwrap_or_default();
                let hits = match service.search(
                    &read_actor,
                    scope,
                    lanes,
                    &q,
                    args.limit.max(1),
                    args.action == "vector_search",
                ) {
                    Ok(hits) => hits,
                    Err(ServiceError::Unavailable(reason))
                        if reason == leveler_memory::MARKDOWN_SEARCH_UNAVAILABLE =>
                    {
                        return Ok(ToolOutput::error(
                            "search is not available on the Markdown memory backend until \
                             Stage 3; use list or read",
                        ));
                    }
                    Err(error) => return Err(service_error(self.name(), error)),
                };
                if hits.is_empty() {
                    return Ok(ToolOutput::ok("No matching memories.".to_string()));
                }
                let mut body = String::new();
                for (view, score) in hits {
                    body.push_str(&format!(
                        "- [{}] {} (score {:.2})\n  {}\n",
                        view.id,
                        view.title,
                        score,
                        view.body.lines().next().unwrap_or("")
                    ));
                }
                Ok(ToolOutput::ok(body))
            }
            "update" => {
                let actor = model_actor(&context)?;
                let id = require_id(&args)?;
                let expected = require_revision(&args)?;
                if args.title.is_none()
                    && args.body.is_none()
                    && args.value.is_none()
                    && args.tags.is_none()
                    && args.expires_at.is_none()
                {
                    return Ok(ToolOutput::error(
                        "update requires at least one of title, body, value, tags, expires_at",
                    ));
                }
                let patch = UpdateRequest {
                    title: args.title,
                    body: args.body,
                    value: args.value,
                    tags: args.tags,
                    expires_at: args.expires_at.map(Some),
                    // Only a person may attach a structured identity to their
                    // own explicit memory; the model tool cannot rename an
                    // existing fact's identity.
                    subject: None,
                };
                let outcome = service
                    .update(&actor, scope, &id, expected, patch)
                    .map_err(|error| service_error(self.name(), error))?;
                Ok(ToolOutput::ok(outcome_message(&outcome)))
            }
            "deprecate" => {
                let actor = model_actor(&context)?;
                let id = require_id(&args)?;
                let expected = require_revision(&args)?;
                let reason = args.reason.unwrap_or_else(|| "deprecated".into());
                let outcome = service
                    .deprecate(&actor, scope, &id, expected, reason)
                    .map_err(|error| service_error(self.name(), error))?;
                Ok(ToolOutput::ok(outcome_message(&outcome)))
            }
            "pin" | "unpin" => {
                let actor = model_actor(&context)?;
                let id = require_id(&args)?;
                let pinned = args.action == "pin";
                let outcome = service
                    .pin(&actor, scope, &id, pinned)
                    .map_err(|error| service_error(self.name(), error))?;
                Ok(ToolOutput::ok(outcome_message(&outcome)))
            }
            other => Ok(ToolOutput::error(format!(
                "unknown action `{other}`; use list|read|search|vector_search|update|deprecate|pin|unpin"
            ))),
        }
    }
}

fn require_id(args: &MemoryArgs) -> Result<String, ToolError> {
    args.id.clone().ok_or_else(|| ToolError::InvalidArguments {
        tool: "memory".into(),
        message: "this action requires id".into(),
    })
}

fn require_revision(args: &MemoryArgs) -> Result<u64, ToolError> {
    args.expected_revision
        .ok_or_else(|| ToolError::InvalidArguments {
            tool: "memory".into(),
            message: "this action requires expected_revision (read the record first)".into(),
        })
}

#[derive(Debug, Deserialize, JsonSchema)]
struct RememberArgs {
    title: String,
    body: String,
    /// The durable subject this fact is about, e.g. `project.default_model`.
    subject: String,
    /// How this memory reaches future turns.
    kind: ToolMemoryKind,
    #[serde(default)]
    tags: Vec<String>,
    /// The structured value of the subject, kept apart from the prose body.
    #[serde(default)]
    value: Option<String>,
    /// Absolute RFC3339 instant after which the fact is no longer current.
    #[serde(default)]
    expires_at: Option<String>,
    /// A rationale kept by the legacy store; not evidence about the user.
    #[serde(default)]
    evidence: Option<String>,
    /// Storage scope: project (default) | local | global.
    #[serde(default)]
    scope: Option<String>,
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
    /// A fact about the user or the project.
    UserFact,
}

impl ToolMemoryKind {
    fn as_str(&self) -> &'static str {
        match self {
            Self::Preference => "preference",
            Self::Decision => "decision",
            Self::Note => "note",
            Self::UserFact => "user_fact",
        }
    }
}

impl From<ToolMemoryKind> for leveler_memory::MemoryKind {
    fn from(value: ToolMemoryKind) -> Self {
        match value {
            ToolMemoryKind::Preference => Self::Preference,
            ToolMemoryKind::Decision => Self::Decision,
            ToolMemoryKind::Note | ToolMemoryKind::UserFact => Self::Note,
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
        "Propose a durable memory: `title`, `body`, `subject`, and `kind` \
         (`user_fact`, `preference`, `decision`, or `note`). Optional `value` \
         is the structured value of the subject; optional RFC3339 `expires_at` \
         makes the fact stop being current at that instant. `subject` is the stable fact \
         identity (e.g. `project.default_model`): a later value for the same \
         subject updates the current memory instead of adding a second entry. \
         Approval, authority and pending behaviour are governed by the Memory \
         section of the system prompt; a change to a user-owned fact is queued \
         for the user. \
         `preference` is injected into later turns; `decision` and `note` are \
         retrieved by relevance or title. `forget` archives an id. Secrets are \
         not a valid body."
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
        if !self.root.lanes().user {
            return Ok(ToolOutput::error(
                "user memory lane is disabled for this project",
            ));
        }
        if args.title.trim().is_empty() || args.body.trim().is_empty() {
            return Ok(ToolOutput::error("title and body are required"));
        }
        let service = self.root.service()?;
        let scope = parse_scope(args.scope.as_deref())?;
        let actor = model_actor(&context)?;
        let tool_call = context
            .tool_call_scope
            .as_deref()
            .expect("model_actor requires a call identity");
        let request = RememberRequest {
            // Provider tool-call ids are opaque; derive a journal-safe
            // spelling that still identifies this exact call on retry.
            op_id: leveler_memory::v2::OpId::derived("remember", tool_call),
            scope,
            title: args.title,
            body: args.body,
            kind: args.kind.as_str().to_string(),
            subject: Some(args.subject),
            value: args.value,
            tags: args.tags,
            evidence: args.evidence,
            expires_at: args.expires_at,
            replaces_id: None,
        };
        let outcome = service
            .remember(&actor, request)
            .map_err(|error| service_error(self.name(), error))?;
        Ok(ToolOutput::ok(outcome_message(&outcome)))
    }
}

#[derive(Debug, Deserialize, JsonSchema)]
struct ForgetArgs {
    id: String,
    #[serde(default)]
    scope: Option<String>,
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
        "Archive a memory by id. The entry is retained for audit and is no \
         longer active. Requires user approval. Archiving does not write a \
         replacement; a replacement is a separate `remember` proposal. A \
         user-owned memory is queued for the user instead of being removed."
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
        let service = self.root.service()?;
        let scope = parse_scope(args.scope.as_deref())?;
        let view = match service.read(
            &leveler_memory::v2::Actor::External,
            scope,
            &args.id,
        ) {
            Ok(view) => view,
            Err(ServiceError::NotFound(_)) => {
                return Ok(ToolOutput::error("memory not found"));
            }
            Err(error) => return Err(service_error(self.name(), error)),
        };
        if !self
            .root
            .lanes()
            .allows_experience(view.is_experience())
        {
            return Ok(ToolOutput::error("memory not found"));
        }
        let actor = model_actor(&context)?;
        let outcome = service
            .forget(&actor, scope, &args.id)
            .map_err(|error| service_error(self.name(), error))?;
        Ok(ToolOutput::ok(outcome_message(&outcome)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use leveler_memory::MemoryStore;
    use tempfile::tempdir;

    fn ctx_in(root: &std::path::Path) -> ToolContext {
        use std::sync::atomic::{AtomicU64, Ordering};
        static CALLS: AtomicU64 = AtomicU64::new(0);
        let call = CALLS.fetch_add(1, Ordering::Relaxed);
        let ws = leveler_execution::Workspace::new(root).unwrap();
        ToolContext::new(ws, leveler_execution::PermissionProfile::Assisted)
            .with_session_scope("test-session")
            .with_turn_scope("test-turn")
            .with_tool_call_scope(format!("test-call-{call}"))
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

    /// Provider tool-call ids are opaque; the remembered operation id must
    /// still be journal-safe. A call id outside `[A-Za-z0-9_\-:.]` must write
    /// instead of failing with `invalid operation id`, and a retry of the exact
    /// same call must replay rather than write twice.
    #[tokio::test]
    async fn an_opaque_tool_call_id_still_writes_and_replays() {
        let dir = tempdir().unwrap();
        let current = dir.path().join("memory");
        let state = dir.path().join("state");
        let service = Arc::new(leveler_memory::MemoryService::new(
            leveler_memory::MemoryRoots::markdown().with_project(
                leveler_memory::v2::ScopeRoots::new(
                    leveler_memory::v2::MemoryScope::Project,
                    current.clone(),
                    state,
                ),
            ),
        ));
        let ws = leveler_execution::Workspace::new(dir.path()).unwrap();
        let ctx = ToolContext::new(ws, leveler_execution::PermissionProfile::Assisted)
            .with_session_scope("test-session")
            .with_turn_scope("test-turn")
            .with_tool_call_scope("call|opaque/with spaces");
        let tool = RememberTool::new(MemoryRoot::from_service(Some(service)));
        let arguments = serde_json::json!({
            "title": "用 pnpm",
            "body": "本仓库统一用 pnpm，不要用 npm。",
            "kind": "preference",
            "subject": "package_manager",
            "value": "pnpm",
        });
        let out = tool
            .execute(arguments.clone(), ctx.clone(), CancellationToken::new())
            .await
            .unwrap();
        assert!(!out.is_error, "{}", out.content);
        let replayed = tool
            .execute(arguments, ctx, CancellationToken::new())
            .await
            .unwrap();
        assert!(!replayed.is_error, "{}", replayed.content);
        assert!(
            replayed.content.contains("replayed; already applied"),
            "a retried call is the same operation: {}",
            replayed.content
        );
        let active: Vec<_> = std::fs::read_dir(&current)
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .filter(|path| path.extension().is_some_and(|ext| ext == "md"))
            .collect();
        assert_eq!(active.len(), 1, "the retry must not write twice");
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
        let key = leveler_memory::semantic_key_of("default_model");
        let mut user = leveler_memory::MemoryCandidate::new(
            "项目默认模型",
            "以后本项目默认模型固定为 Pro",
            leveler_memory::CandidateKind::Free,
            Some(key.clone()),
            leveler_memory::CandidateSource::UserExplicit,
            vec![],
        )
        .unwrap();
        user.semantic_key = Some(key);
        user.authority = leveler_memory::MemoryAuthority::ExplicitUser;
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

    fn experience_entry() -> leveler_memory::MemoryEntry {
        let mut entry = leveler_memory::new_entry(
            "Deploy lesson",
            "The blue deployment failed because the token had expired.",
            Vec::new(),
        );
        entry.id = "deploy-lesson".to_string();
        entry.kind = Some(leveler_memory::EXPERIENCE_KIND.to_string());
        entry.experience = Some(leveler_memory::ExperienceRecord {
            category: leveler_memory::ExperienceCategory::RootCause,
            verification: leveler_memory::ExperienceVerification::Verified,
            applies_when: "deploying to blue".to_string(),
            invalid_if: None,
            evidence: vec![leveler_memory::ExperienceEvidence {
                kind: "failure".to_string(),
                reference: "step 2: deploy (exit 1)".to_string(),
            }],
        });
        entry
    }

    /// A recall candidate carries only the row, so the read that expands it
    /// must carry everything a reader needs to judge the lesson: the body, the
    /// standing, and both applicability clauses.
    #[tokio::test]
    async fn reading_an_experience_returns_its_standing_and_conditions() {
        let dir = tempdir().unwrap();
        let mem = dir.path().join("memory");
        let root = MemoryRoot::new(Some(mem.clone()));
        let ctx = ctx_in(dir.path());
        let store = MemoryStore::open(&mem).unwrap();
        let mut entry = experience_entry();
        entry.experience.as_mut().unwrap().invalid_if =
            Some("the deployment target changes".to_string());
        store.remember(entry).unwrap();

        let read = MemoryTool::new(root)
            .execute(
                serde_json::json!({"action": "read", "id": "deploy-lesson"}),
                ctx,
                CancellationToken::new(),
            )
            .await
            .unwrap();
        assert!(!read.is_error, "{}", read.content);
        assert!(
            read.content.contains("token had expired"),
            "{}",
            read.content
        );
        assert!(
            read.content.contains("experience·observed"),
            "the fixture has failure evidence but no resolution: {}",
            read.content
        );
        assert!(
            !read.content.contains("experience·verified"),
            "an observation-only lesson must not read as a resolution: {}",
            read.content
        );
        assert!(
            read.content.contains("applies when: deploying to blue"),
            "{}",
            read.content
        );
        assert!(
            read.content
                .contains("invalid if: the deployment target changes"),
            "{}",
            read.content
        );
    }

    #[tokio::test]
    async fn memory_reads_never_cross_a_closed_lane() {
        let dir = tempdir().unwrap();
        let mem = dir.path().join("memory");
        let ctx = ctx_in(dir.path());
        let store = MemoryStore::open(&mem).unwrap();
        let mut user = leveler_memory::new_entry(
            "Release rule",
            "Tag only after the full suite passes.",
            Vec::new(),
        );
        user.kind = Some("decision".to_string());
        store.remember(user).unwrap();
        store.remember(experience_entry()).unwrap();

        let user_only =
            MemoryRoot::new(Some(mem.clone())).with_lanes(leveler_memory::MemoryLanes::USER_ONLY);
        let listed = MemoryTool::new(user_only.clone())
            .execute(
                serde_json::json!({"action": "list"}),
                ctx.clone(),
                CancellationToken::new(),
            )
            .await
            .unwrap();
        assert!(
            listed.content.contains("Release rule"),
            "{}",
            listed.content
        );
        assert!(
            !listed.content.contains("Deploy lesson"),
            "{}",
            listed.content
        );
        let searched = MemoryTool::new(user_only.clone())
            .execute(
                serde_json::json!({"action": "search", "query": "blue deployment", "limit": 10}),
                ctx.clone(),
                CancellationToken::new(),
            )
            .await
            .unwrap();
        assert!(
            !searched.content.contains("deploy-lesson"),
            "{}",
            searched.content
        );
        let read = MemoryTool::new(user_only)
            .execute(
                serde_json::json!({"action": "read", "id": "deploy-lesson"}),
                ctx.clone(),
                CancellationToken::new(),
            )
            .await
            .unwrap();
        assert!(read.is_error, "a disabled lane is not readable");

        let experience_only =
            MemoryRoot::new(Some(mem)).with_lanes(leveler_memory::MemoryLanes::EXPERIENCE_ONLY);
        let listed = MemoryTool::new(experience_only)
            .execute(
                serde_json::json!({"action": "list"}),
                ctx,
                CancellationToken::new(),
            )
            .await
            .unwrap();
        assert!(
            listed.content.contains("Deploy lesson"),
            "{}",
            listed.content
        );
        assert!(
            !listed.content.contains("Release rule"),
            "{}",
            listed.content
        );
    }

    #[tokio::test]
    async fn remember_is_refused_when_the_user_lane_is_closed() {
        let dir = tempdir().unwrap();
        let mem = dir.path().join("memory");
        let root = MemoryRoot::new(Some(mem.clone()))
            .with_lanes(leveler_memory::MemoryLanes::EXPERIENCE_ONLY);
        let out = RememberTool::new(root)
            .execute(
                serde_json::json!({
                    "title": "Release rule",
                    "body": "Tag only after the test suite passes.",
                    "kind": "decision",
                    "subject": "release.tagging"
                }),
                ctx_in(dir.path()),
                CancellationToken::new(),
            )
            .await
            .unwrap();
        assert!(out.is_error, "{}", out.content);
        assert!(
            MemoryStore::open(&mem)
                .unwrap()
                .effective_active()
                .unwrap()
                .is_empty()
        );
    }

    #[tokio::test]
    async fn forget_is_refused_for_an_entry_in_a_closed_lane() {
        let dir = tempdir().unwrap();
        let mem = dir.path().join("memory");
        let store = MemoryStore::open(&mem).unwrap();
        store.remember(experience_entry()).unwrap();
        let root =
            MemoryRoot::new(Some(mem.clone())).with_lanes(leveler_memory::MemoryLanes::USER_ONLY);
        let out = ForgetTool::new(root)
            .execute(
                serde_json::json!({"id": "deploy-lesson"}),
                ctx_in(dir.path()),
                CancellationToken::new(),
            )
            .await
            .unwrap();
        assert!(out.is_error, "{}", out.content);
        assert_eq!(store.effective_active().unwrap().len(), 1);
    }
}
