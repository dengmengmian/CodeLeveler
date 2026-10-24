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

/// A single read never returns more than this much body, independent of the
/// token budget. A larger body is continued with an explicit offset.
const READ_BODY_MAX_BYTES: usize = 64 * 1024;
/// One read may spend about this many estimated tokens.
const READ_CALL_TOKEN_BUDGET: u64 = 600;
/// A turn may issue this many reads ...
const READ_CALLS_PER_TURN: u32 = 4;
/// ... and spend about this many estimated tokens across them.
const READ_TOKENS_PER_TURN: u64 = 8_000;
/// Below this remaining allowance a read cannot make progress; the caller is
/// told the budget is exhausted instead of receiving an empty continuation.
const READ_MIN_ALLOWANCE: u64 = 64;

/// The memory service and lane policy a memory tool is built with.
#[derive(Clone)]
pub struct MemoryRoot {
    service: Option<Arc<MemoryService>>,
    lanes: leveler_memory::MemoryLanes,
}

impl MemoryRoot {
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
                "memory is not configured for this session (the host must hand over a service)"
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
        WriteOutcome::Pending { id, title, reason } => format!(
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
        WriteOutcome::Forgotten { id, title } => {
            format!("Forgotten [{id}]: {title} (the text is deleted; only a tombstone remains)")
        }
    }
}

fn render_view(view: &MemoryView, with_body: bool, scope: ServiceScope) -> String {
    if !with_body {
        return format!("- [{}] {}", view.id, view.title);
    }
    render_view_body(view, scope, &view.body, None)
}

/// Render one record with an explicit body slice. `truncated` is
/// `(start_byte, end_byte, total_bytes)` and adds the continuation contract:
/// a caller must always be able to fetch the rest, never lose the long tail.
fn render_view_body(
    view: &MemoryView,
    scope: ServiceScope,
    body_text: &str,
    truncated: Option<(usize, usize, usize)>,
) -> String {
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
        view.title, view.id, view.status, scope, view.authority, body_text
    );
    if let Some((start, end, total)) = truncated {
        body.push_str(&format!(
            "\n\n(truncated: showed bytes {start}..{end} of {total}; continue with offset={end})"
        ));
    }
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
    /// Action: search | list | read | update | deprecate | pin | unpin
    action: String,
    /// Storage scope: project (default) | local | global.
    #[serde(default)]
    scope: Option<String>,
    /// Search query (action=search) or entry id (action=read).
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
    /// Read action only: byte offset into the body, from a previous
    /// `(truncated: ... continue with offset=N)` marker.
    #[serde(default)]
    offset: Option<usize>,
}

fn default_limit() -> usize {
    5
}

/// What one turn has already spent on full-memory reads.
#[derive(Debug, Default, Clone, Copy)]
struct TurnReadBudget {
    calls: u32,
    tokens: u64,
}

pub struct MemoryTool {
    root: MemoryRoot,
    /// Keyed by (session, turn): a new turn starts with a fresh allowance,
    /// while every call in the same turn shares one bounded budget.
    reads: std::sync::Mutex<std::collections::HashMap<(String, String), TurnReadBudget>>,
}

impl MemoryTool {
    pub fn new(root: MemoryRoot) -> Self {
        Self {
            root,
            reads: std::sync::Mutex::new(std::collections::HashMap::new()),
        }
    }
}

#[async_trait]
impl Tool for MemoryTool {
    fn name(&self) -> &'static str {
        "memory"
    }

    fn description(&self) -> &'static str {
        "Read and maintain durable memories. action=list|read|search|update|deprecate|pin|unpin. \
         Bodies are not in the system prompt — retrieve them here. search runs over the derived \
         lexical index of the current Markdown records. update and deprecate require the revision \
         the caller read. A write to a record the user owns is queued for the user; the model \
         cannot accept or reject pending memory."
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
                            self.bounded_read(&view, scope, args.offset, &context)
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
            "search" => {
                let q = args.query.unwrap_or_default();
                let hits = service
                    .search(&read_actor, scope, lanes, &q, args.limit.max(1))
                    .map_err(|error| service_error(self.name(), error))?;
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
                "unknown action `{other}`; use list|read|search|update|deprecate|pin|unpin"
            ))),
        }
    }
}

impl MemoryTool {
    /// Render a full record under the per-call and per-turn read budget.
    ///
    /// The budget is a resource bound, not a content decision: when it runs
    /// out the caller is told explicitly and receives the offset needed to
    /// continue, so a long record is never silently cut short.
    fn bounded_read(
        &self,
        view: &MemoryView,
        scope: ServiceScope,
        offset: Option<usize>,
        context: &ToolContext,
    ) -> Result<ToolOutput, ToolError> {
        let key = (
            context.session_scope.as_deref().unwrap_or("-").to_string(),
            context.turn_scope.as_deref().unwrap_or("-").to_string(),
        );
        let mut budgets = self
            .reads
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        // Only the current turn's budget is live; older turns would otherwise
        // accumulate forever in a long session.
        budgets.retain(|(session, turn), _| session == &key.0 && turn == &key.1);
        let budget = budgets.entry(key).or_default();
        let remaining_tokens = READ_TOKENS_PER_TURN.saturating_sub(budget.tokens);
        if budget.calls >= READ_CALLS_PER_TURN || remaining_tokens < READ_MIN_ALLOWANCE {
            return Ok(ToolOutput::error(format!(
                "read_budget_exhausted: this turn already used {} memory read(s) and {} tokens. \
                 Continue with the text already returned, or read again in a new turn. \
                 For a truncated record, the last marker carries its continuation offset.",
                budget.calls, budget.tokens
            )));
        }
        let total = view.body.len();
        let requested = offset.unwrap_or(0);
        if requested > total {
            return Ok(ToolOutput::error(format!(
                "offset {requested} is past the end of the body ({total} bytes)"
            )));
        }
        // An offset a caller typed by hand may land inside a character; the
        // slice itself must still start on a UTF-8 boundary.
        let start = char_boundary_floor(&view.body, requested);
        let allowance = READ_CALL_TOKEN_BUDGET.min(remaining_tokens);
        let (text, used) = bounded_read_text(view, scope, start, allowance, READ_BODY_MAX_BYTES);
        budget.calls += 1;
        budget.tokens = budget.tokens.saturating_add(used);
        Ok(ToolOutput::ok(text))
    }
}

/// Shrink a body slice to the largest prefix that fits the token allowance,
/// always honouring UTF-8 boundaries and always making progress.
fn bounded_read_text(
    view: &MemoryView,
    scope: ServiceScope,
    start: usize,
    allowance: u64,
    max_bytes: usize,
) -> (String, u64) {
    let total = view.body.len();
    let hard_end = char_boundary_floor(&view.body, (start + max_bytes).min(total));
    let mut end = hard_end;
    loop {
        let text = render_slice(view, scope, start, end);
        let used = leveler_model::estimate_text(&text);
        if used <= allowance || end <= start {
            // Ensure at least one character when the body still has content:
            // a continuation that cannot advance would trap the caller.
            if start < total && end == start {
                end = next_char_boundary(&view.body, start + 1);
                let text = render_slice(view, scope, start, end);
                return (text.clone(), leveler_model::estimate_text(&text));
            }
            return (text, used);
        }
        let chars = view.body[start..end].chars().count();
        if chars <= 1 {
            let text = render_slice(view, scope, start, end);
            return (text.clone(), leveler_model::estimate_text(&text));
        }
        let keep = (chars * 4 / 5).max(1);
        end = start
            + view.body[start..]
                .char_indices()
                .nth(keep)
                .map(|(index, _)| index)
                .unwrap_or(total - start);
    }
}

fn render_slice(view: &MemoryView, scope: ServiceScope, start: usize, end: usize) -> String {
    let total = view.body.len();
    let truncated = (end < total).then_some((start, end, total));
    render_view_body(view, scope, &view.body[start..end], truncated)
}

fn char_boundary_floor(text: &str, index: usize) -> usize {
    let mut index = index.min(text.len());
    while index > 0 && !text.is_char_boundary(index) {
        index -= 1;
    }
    index
}

fn next_char_boundary(text: &str, index: usize) -> usize {
    let mut index = index.min(text.len());
    loop {
        if index >= text.len() || text.is_char_boundary(index) {
            return index;
        }
        index += 1;
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
         retrieved by relevance or title. `forget` deletes an id's text for \
         good. Secrets are not a valid body."
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
            // A model cannot supply evidence about the user: a quoted span it
            // writes is its own text, not a record of what the user said. Real
            // user-turn evidence is stamped by the runtime that observed the
            // turn, so this field stays empty here.
            evidence: None,
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
        "Forget a memory by id for good: its text, its revisions and its index \
         rows are deleted, and only a minimal tombstone remains. This is not \
         reversible and not the same as retiring an entry while keeping its \
         history. Requires user approval. A user-owned memory is queued for the \
         user instead of being deleted."
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
        let view = match service.read(&leveler_memory::v2::Actor::External, scope, &args.id) {
            Ok(view) => view,
            Err(ServiceError::NotFound(_)) => {
                return Ok(ToolOutput::error("memory not found"));
            }
            Err(error) => return Err(service_error(self.name(), error)),
        };
        if !self.root.lanes().allows_experience(view.is_experience()) {
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
    use leveler_memory::v2::{MemoryScope, ScopeRoots, UserInvocationId};
    use tempfile::tempdir;

    /// A project-scoped Markdown store plus the tool handle over it.
    fn fixture(dir: &std::path::Path) -> (Arc<MemoryService>, MemoryRoot) {
        let service = Arc::new(MemoryService::new(
            leveler_memory::MemoryRoots::new().with_project(ScopeRoots::new(
                MemoryScope::Project,
                dir.join("current"),
                dir.join("state"),
            )),
        ));
        let root = MemoryRoot::from_service(Some(service.clone()));
        (service, root)
    }

    fn user() -> leveler_memory::v2::Actor {
        use std::sync::atomic::{AtomicU64, Ordering};
        static NEXT: AtomicU64 = AtomicU64::new(0);
        leveler_memory::v2::Actor::User {
            invocation: UserInvocationId::mint(format!(
                "tools-test-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            )),
        }
    }

    /// One explicit user fact, written through the same facade production uses.
    fn seed_user(
        service: &MemoryService,
        title: &str,
        body: &str,
        kind: &str,
        subject: Option<&str>,
    ) -> String {
        let mut request = RememberRequest::new(
            leveler_memory::v2::OpId::derived("seed", title),
            ServiceScope::Project,
            title,
            body,
            kind,
        );
        request.subject = subject.map(str::to_string);
        service
            .remember(&user(), request)
            .unwrap()
            .applied_id()
            .expect("user seed must apply")
            .to_string()
    }

    /// One runtime-observed lesson, with the evidence the store requires.
    fn seed_experience(
        service: &MemoryService,
        verified: bool,
        invalid_if: Option<&str>,
    ) -> String {
        use leveler_memory::{
            CandidateKind, CandidateSource, ExperienceCategory, ExperienceEvidence,
            ExperienceRecord, ExperienceVerification, MemoryCandidate,
        };
        let mut candidate = MemoryCandidate::new(
            "Deploy lesson",
            "The blue deployment failed because the token had expired.",
            CandidateKind::Free,
            None,
            CandidateSource::RuntimeObserved,
            Vec::new(),
        )
        .unwrap();
        let mut evidence = vec![ExperienceEvidence {
            kind: "failure".to_string(),
            reference: "step 2: deploy (exit 1)".to_string(),
        }];
        if verified {
            evidence.push(ExperienceEvidence {
                kind: "resolution".to_string(),
                reference: "step 4: rotate the token (exit 0)".to_string(),
            });
        }
        candidate.experience = Some(ExperienceRecord {
            category: ExperienceCategory::RootCause,
            verification: if verified {
                ExperienceVerification::Verified
            } else {
                ExperienceVerification::Unverified
            },
            applies_when: "deploying to blue".to_string(),
            invalid_if: invalid_if.map(str::to_string),
            evidence,
        });
        let actor = leveler_memory::v2::Actor::Runtime {
            task: leveler_core::TaskId::new("fixture-task"),
        };
        service
            .admit_candidate(&actor, ServiceScope::Project, &candidate, "seed-experience")
            .unwrap()
            .applied_id()
            .expect("experience seed must apply")
            .to_string()
    }

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

    fn active_ids(service: &MemoryService) -> Vec<String> {
        service
            .listing_for_user(ServiceScope::Project, leveler_memory::MemoryLanes::ALL)
            .unwrap()
            .into_iter()
            .map(|view| view.id)
            .collect()
    }

    /// The store a memory tool writes to is the one it was CONSTRUCTED with —
    /// never an ambient environment variable, and never a location invented at
    /// call time.
    #[tokio::test]
    async fn writes_to_the_root_it_was_constructed_with() {
        let dir = tempdir().unwrap();
        let (service, root) = fixture(dir.path());
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
        let records: Vec<_> = std::fs::read_dir(dir.path().join("current"))
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .filter(|path| path.extension().is_some_and(|ext| ext == "md"))
            .collect();
        assert_eq!(records.len(), 1, "one Markdown record: {records:?}");
        let listed = MemoryTool::new(root)
            .execute(
                serde_json::json!({"action": "list"}),
                ctx,
                CancellationToken::new(),
            )
            .await
            .unwrap();
        assert!(listed.content.contains("Prefer workspace-write"));
        assert_eq!(active_ids(&service).len(), 1);
    }

    /// Provider tool-call ids are opaque; the remembered operation id must
    /// still be journal-safe. A call id outside `[A-Za-z0-9_\-:.]` must write
    /// instead of failing with `invalid operation id`, and a retry of the exact
    /// same call must replay rather than write twice.
    #[tokio::test]
    async fn an_opaque_tool_call_id_still_writes_and_replays() {
        let dir = tempdir().unwrap();
        let (service, root) = fixture(dir.path());
        let ws = leveler_execution::Workspace::new(dir.path()).unwrap();
        let ctx = ToolContext::new(ws, leveler_execution::PermissionProfile::Assisted)
            .with_session_scope("test-session")
            .with_turn_scope("test-turn")
            .with_tool_call_scope("call|opaque/with spaces");
        let tool = RememberTool::new(root);
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
        assert_eq!(
            active_ids(&service).len(),
            1,
            "the retry must not write twice"
        );
    }

    #[tokio::test]
    async fn two_different_facts_with_the_same_title_do_not_clobber_each_other() {
        let dir = tempdir().unwrap();
        let (service, root) = fixture(dir.path());
        let ctx = ctx_in(dir.path());
        let dir_for_call = dir.path().to_path_buf();
        let remember = |body: &'static str, subject: &'static str| {
            // A distinct tool call per write: one call identity is one
            // operation, and two different facts are two operations.
            let ctx = ctx_in(&dir_for_call);
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
        let listed = MemoryTool::new(root.clone())
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
        assert_eq!(active_ids(&service).len(), 2);
    }

    /// A run with memory unconfigured says so. Nothing invents a store
    /// location, and nothing silently succeeds against one.
    #[tokio::test]
    async fn an_unconfigured_memory_root_errors_clearly() {
        let dir = tempdir().unwrap();
        let ctx = ctx_in(dir.path());
        let err = MemoryTool::new(MemoryRoot::from_service(None))
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
    async fn search_returns_ranked_hits() {
        let dir = tempdir().unwrap();
        let (_service, root) = fixture(dir.path());
        let ctx = ctx_in(dir.path());
        RememberTool::new(root.clone())
            .execute(
                serde_json::json!({"title": "Workspace write", "body": "Prefer PermissionProfile::Assisted for file edits.", "kind": "note", "subject": "editing.permission_profile"}),
                ctx_in(dir.path()),
                CancellationToken::new(),
            )
            .await
            .unwrap();
        RememberTool::new(root.clone())
            .execute(
                serde_json::json!({"title": "Unrelated", "body": "The sky is blue on clear days.", "kind": "note", "subject": "weather.sky"}),
                ctx_in(dir.path()),
                CancellationToken::new(),
            )
            .await
            .unwrap();
        let out = MemoryTool::new(root)
            .execute(
                serde_json::json!({
                    "action": "search",
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
        let (service, root) = fixture(dir.path());
        let ctx = ctx_in(dir.path());
        let user_id = seed_user(
            &service,
            "Deploy notes",
            "User chose the release branch.",
            "note",
            Some("deploy.release"),
        );

        let out = RememberTool::new(root)
            .execute(
                serde_json::json!({
                    "title": "Deploy notes",
                    "body": "The model thinks production is gated on on-call approval.",
                    "kind": "note",
                    "subject": "deploy.production"
                }),
                ctx,
                CancellationToken::new(),
            )
            .await
            .unwrap();
        assert!(!out.is_error, "{}", out.content);

        let active = service
            .listing_for_user(ServiceScope::Project, leveler_memory::MemoryLanes::ALL)
            .unwrap();
        assert_eq!(active.len(), 2, "the model adds a second entry: {active:?}");
        let user_view = active.iter().find(|view| view.id == user_id).unwrap();
        assert_eq!(user_view.authority, "user_explicit");
        let model_view = active
            .iter()
            .find(|view| view.id != user_id)
            .expect("model entry");
        assert_eq!(
            model_view.authority, "model_inferred",
            "approval to execute is not a claim that the user asserted the fact"
        );
        // The user's record keeps its own body and revision.
        let read = service
            .read(
                &leveler_memory::v2::Actor::External,
                ServiceScope::Project,
                &user_id,
            )
            .unwrap();
        assert_eq!(read.body, "User chose the release branch.");
        assert_eq!(read.revision, Some(1));
    }

    /// The same semantic subject is one identity, so a second model value does
    /// not accumulate beside the first: with no evidence of its own, a model
    /// tool call cannot silently replace a current value and is queued for a
    /// person instead.
    #[tokio::test]
    async fn agent_remember_with_the_same_subject_waits_instead_of_duplicating() {
        let dir = tempdir().unwrap();
        let (service, root) = fixture(dir.path());
        let dir_for_call = dir.path().to_path_buf();
        let remember = |body: &'static str| {
            let ctx = ctx_in(&dir_for_call);
            let root = root.clone();
            async move {
                RememberTool::new(root)
                    .execute(
                        serde_json::json!({
                            "title": "项目默认模型",
                            "body": body,
                            "kind": "note",
                            "subject": "project.default_model"
                        }),
                        ctx,
                        CancellationToken::new(),
                    )
                    .await
                    .unwrap()
            }
        };

        let first = remember("项目默认模型是 Pro").await;
        assert!(!first.is_error, "{}", first.content);
        let second = remember("项目默认模型是 Flash").await;
        assert!(!second.is_error, "{}", second.content);
        assert!(
            second.content.contains("approval") || second.content.contains("等待"),
            "a replacement without new evidence waits for a person: {}",
            second.content
        );

        let active = service
            .listing_for_user(ServiceScope::Project, leveler_memory::MemoryLanes::ALL)
            .unwrap();
        assert_eq!(active.len(), 1, "one current truth: {active:?}");
        let current = service
            .read(
                &leveler_memory::v2::Actor::External,
                ServiceScope::Project,
                &active[0].id,
            )
            .unwrap();
        assert!(current.body.contains("Pro"));
        let pending = service.pending(&user(), ServiceScope::Project).unwrap();
        assert_eq!(pending.len(), 1, "the new value is a proposal");
        assert!(
            pending[0].body.contains("Flash"),
            "the proposal keeps the new value: {pending:?}"
        );
    }

    /// Approval to execute a tool is not a claim that the model's fact
    /// outranks the user's. A conflicting agent value waits for a person.
    #[tokio::test]
    async fn agent_remember_conflicting_with_a_user_fact_waits_for_approval() {
        let dir = tempdir().unwrap();
        let (service, root) = fixture(dir.path());
        let ctx = ctx_in(dir.path());
        let user_id = seed_user(
            &service,
            "项目默认模型",
            "以后本项目默认模型固定为 Pro",
            "note",
            Some("project.default_model"),
        );

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
        let active = service
            .listing_for_user(ServiceScope::Project, leveler_memory::MemoryLanes::ALL)
            .unwrap();
        assert_eq!(active.len(), 1);
        assert_eq!(active[0].id, user_id);
        let current = service
            .read(
                &leveler_memory::v2::Actor::External,
                ServiceScope::Project,
                &user_id,
            )
            .unwrap();
        assert!(current.body.contains("Pro"), "{current:?}");
        assert_eq!(
            service
                .archived(
                    &leveler_memory::v2::Actor::External,
                    ServiceScope::Project,
                    leveler_memory::MemoryLanes::ALL,
                    None,
                )
                .unwrap()
                .len(),
            0
        );
        assert_eq!(
            service
                .pending(&user(), ServiceScope::Project)
                .unwrap()
                .len(),
            1,
            "conflict is kept"
        );
    }

    /// Every model-facing read presents current truth. A rejected proposal or a
    /// superseded record is history: `list` and `read` must not present it.
    #[tokio::test]
    async fn reads_never_present_history_as_current_truth() {
        let dir = tempdir().unwrap();
        let (service, root) = fixture(dir.path());
        let ctx = ctx_in(dir.path());

        // The correction is the user's, so it replaces the earlier value and
        // the old record becomes history.
        let old_id = seed_user(
            &service,
            "Old value",
            "默认模型：Pro",
            "note",
            Some("project.default_model"),
        );
        seed_user(
            &service,
            "New value",
            "默认模型：Flash",
            "note",
            Some("project.default_model"),
        );

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
                serde_json::json!({"action": "read", "id": old_id}),
                ctx,
                CancellationToken::new(),
            )
            .await
            .unwrap();
        assert!(read.is_error, "a superseded record is not current truth");
    }

    #[tokio::test]
    async fn remember_requires_a_subject_and_preserves_the_declared_kind() {
        let dir = tempdir().unwrap();
        let (service, root) = fixture(dir.path());
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
        let active = service
            .listing_for_user(ServiceScope::Project, leveler_memory::MemoryLanes::ALL)
            .unwrap();
        assert!(
            active[0].kind == "user_fact",
            "kind is the record kind: {active:?}"
        );
        assert!(
            active[0].tags.iter().any(|tag| tag == "decision"),
            "the declared label stays visible as a tag: {active:?}"
        );
    }

    /// A credential-shaped write never reaches the store, so no read action
    /// can leak it. The refusal is the contract; an empty store is the proof.
    #[tokio::test]
    async fn a_credential_shaped_remember_is_refused_and_never_appears() {
        let dir = tempdir().unwrap();
        let (service, root) = fixture(dir.path());
        let ctx = ctx_in(dir.path());
        seed_user(
            &service,
            "Deploy preference",
            "Use foreground service startup.",
            "note",
            Some("deploy.startup"),
        );

        let refused = RememberTool::new(root.clone())
            .execute(
                serde_json::json!({
                    "title": "Provider credential",
                    "body": "API_KEY=demo-provider-value-1234567890",
                    "kind": "note",
                    "subject": "provider.api_key"
                }),
                ctx.clone(),
                CancellationToken::new(),
            )
            .await;
        match refused {
            Err(error) => assert!(error.to_string().contains("credential"), "{error}"),
            Ok(output) => assert!(output.is_error, "{}", output.content),
        }
        assert_eq!(active_ids(&service).len(), 1, "only the safe fact exists");

        for input in [
            serde_json::json!({"action": "list"}),
            serde_json::json!({"action": "search", "query": "API key provider", "limit": 10}),
        ] {
            let out = MemoryTool::new(root.clone())
                .execute(input, ctx.clone(), CancellationToken::new())
                .await
                .unwrap();
            assert!(!out.content.contains("demo-provider"), "{}", out.content);
        }
    }

    /// A recall candidate carries only the row, so the read that expands it
    /// must carry everything a reader needs to judge the lesson: the body, the
    /// standing, and both applicability clauses.
    #[tokio::test]
    async fn reading_an_experience_returns_its_standing_and_conditions() {
        let dir = tempdir().unwrap();
        let (service, root) = fixture(dir.path());
        let ctx = ctx_in(dir.path());
        let id = seed_experience(&service, false, Some("the deployment target changes"));

        let read = MemoryTool::new(root)
            .execute(
                serde_json::json!({"action": "read", "id": id}),
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
            read.content.contains("experience\u{b7}observed"),
            "the fixture has failure evidence but no resolution: {}",
            read.content
        );
        assert!(
            !read.content.contains("experience\u{b7}verified"),
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
        let (service, root) = fixture(dir.path());
        let ctx = ctx_in(dir.path());
        seed_user(
            &service,
            "Release rule",
            "Tag only after the full suite passes.",
            "decision",
            Some("release.tagging"),
        );
        let experience_id = seed_experience(&service, true, None);

        let user_only = root
            .clone()
            .with_lanes(leveler_memory::MemoryLanes::USER_ONLY);
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
            !searched.content.contains(&experience_id),
            "{}",
            searched.content
        );
        let read = MemoryTool::new(user_only)
            .execute(
                serde_json::json!({"action": "read", "id": experience_id}),
                ctx.clone(),
                CancellationToken::new(),
            )
            .await
            .unwrap();
        assert!(read.is_error, "a disabled lane is not readable");

        let experience_only = root.with_lanes(leveler_memory::MemoryLanes::EXPERIENCE_ONLY);
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
        let (service, root) = fixture(dir.path());
        let root = root.with_lanes(leveler_memory::MemoryLanes::EXPERIENCE_ONLY);
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
        assert!(active_ids(&service).is_empty());
    }

    #[tokio::test]
    async fn forget_is_refused_for_an_entry_in_a_closed_lane() {
        let dir = tempdir().unwrap();
        let (service, root) = fixture(dir.path());
        let experience_id = seed_experience(&service, true, None);
        let root = root.with_lanes(leveler_memory::MemoryLanes::USER_ONLY);
        let out = ForgetTool::new(root)
            .execute(
                serde_json::json!({"id": experience_id}),
                ctx_in(dir.path()),
                CancellationToken::new(),
            )
            .await
            .unwrap();
        assert!(out.is_error, "{}", out.content);
        assert_eq!(active_ids(&service).len(), 1);
    }

    /// The body slice inside a rendered read, between the id header and the
    /// first metadata/continuation suffix.
    fn rendered_body(content: &str) -> &str {
        let id_at = content.find("\n\n(id:").expect("id header");
        let body_start = id_at + content[id_at + 2..].find("\n\n").expect("header end") + 4;
        let rest = &content[body_start..];
        let end = [
            "\n\n(truncated",
            "\n\n(tags:",
            "\n\n(revision",
            "\n\n(not current",
            "\n\n(experience",
        ]
        .iter()
        .filter_map(|marker| rest.find(marker))
        .min()
        .unwrap_or(rest.len());
        &rest[..end]
    }

    /// A full read is bounded per call and its continuation marker is the
    /// contract that returns the rest: no silent long-tail loss.
    #[tokio::test]
    async fn a_long_read_is_bounded_and_continued_by_offset() {
        let dir = tempdir().unwrap();
        let (service, root) = fixture(dir.path());
        let body = format!("开头{}结尾", "中".repeat(1_500));
        let id = seed_user(&service, "长记录", &body, "note", None);
        let tool = MemoryTool::new(root);
        let ctx = ctx_in(dir.path());

        let mut collected = String::new();
        let mut offset = 0usize;
        let mut calls = 0;
        let mut saw_truncation = false;
        loop {
            let out = tool
                .execute(
                    serde_json::json!({"action": "read", "id": id, "offset": offset}),
                    ctx.clone(),
                    CancellationToken::new(),
                )
                .await
                .unwrap();
            assert!(!out.is_error, "{}", out.content);
            assert!(
                leveler_model::estimate_text(&out.content) <= READ_CALL_TOKEN_BUDGET + 40,
                "a read exceeded its call budget: {} tokens",
                leveler_model::estimate_text(&out.content)
            );
            calls += 1;
            collected.push_str(rendered_body(&out.content));
            let marker = "(truncated: showed bytes ";
            if let Some(index) = out.content.find(marker) {
                saw_truncation = true;
                let after = &out.content[index + marker.len()..];
                let end_bytes = after.split("..").nth(1).unwrap().split(' ').next().unwrap();
                let end: usize = end_bytes.parse().unwrap();
                assert!(end > offset, "continuation did not advance");
                offset = end;
                assert!(calls < 8, "continuation took too many calls");
            } else {
                break;
            }
        }
        assert!(
            saw_truncation,
            "a 1.5k-character body must not fit one read"
        );
        assert_eq!(collected, body, "continuation lost part of the body");
    }

    #[tokio::test]
    async fn read_budget_is_per_turn_and_exhaustion_is_explicit() {
        let dir = tempdir().unwrap();
        let (service, root) = fixture(dir.path());
        let id = seed_user(&service, "小记录", "很短的内容", "note", None);
        let tool = MemoryTool::new(root);

        for _ in 0..READ_CALLS_PER_TURN {
            let out = tool
                .execute(
                    serde_json::json!({"action": "read", "id": id}),
                    ctx_in(dir.path()),
                    CancellationToken::new(),
                )
                .await
                .unwrap();
            assert!(!out.is_error, "{}", out.content);
        }
        let exhausted = tool
            .execute(
                serde_json::json!({"action": "read", "id": id}),
                ctx_in(dir.path()),
                CancellationToken::new(),
            )
            .await
            .unwrap();
        assert!(exhausted.is_error, "{}", exhausted.content);
        assert!(
            exhausted.content.contains("read_budget_exhausted"),
            "{}",
            exhausted.content
        );

        // A new turn is a new budget.
        let next_turn = ToolContext::new(
            leveler_execution::Workspace::new(dir.path()).unwrap(),
            leveler_execution::PermissionProfile::Assisted,
        )
        .with_session_scope("test-session")
        .with_turn_scope("next-turn")
        .with_tool_call_scope("next-call");
        let out = tool
            .execute(
                serde_json::json!({"action": "read", "id": id}),
                next_turn,
                CancellationToken::new(),
            )
            .await
            .unwrap();
        assert!(!out.is_error, "{}", out.content);
    }
}
