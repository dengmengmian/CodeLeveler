//! Definitions of the executor-injected tools (request_user_input,
//! update_goal, request_permissions, spawn_agent) advertised to the model.
//!
//! `ask_user` has no definition: it is a parser alias only, see
//! [`is_user_input_tool`].

use leveler_model::{ToolCall, ToolDefinition};

/// Primary name for mid-turn clarification.
pub(crate) const REQUEST_USER_INPUT_TOOL: &str = "request_user_input";

/// Legacy alias kept for older prompts / transcripts; same Clarifier path.
pub(crate) const ASK_USER_TOOL: &str = "ask_user";

/// Shared schema for `request_user_input` and `ask_user`.
///
/// `questions` is the multi-question form: one interaction the user answers
/// as a set, shown as tabs. `question`/`options` remain the single-question
/// form so a model that asks one thing keeps working unchanged.
fn user_input_input_schema() -> serde_json::Value {
    serde_json::json!({
        "type": "object",
        "properties": {
            "question": {
                "type": "string",
                "description": "The question to ask. With `questions`, use it as the interaction's one-line headline."
            },
            "options": {
                "type": "array",
                "items": { "type": "string" },
                "description": "2-4 mutually exclusive choices, one short line each. Omit for a free-text answer."
            },
            "questions": {
                "type": "array",
                "description": "Several questions answered in one interaction, one tab each.",
                "items": {
                    "type": "object",
                    "properties": {
                        "header": {
                            "type": "string",
                            "description": "Short tab label (2-6 characters), e.g. 数据策略."
                        },
                        "question": {
                            "type": "string",
                            "description": "The full question, one short sentence."
                        },
                        "kind": {
                            "type": "string",
                            "enum": ["single", "multi", "text"],
                            "description": "single = exactly one of `options`; multi = any number of `options`; text = the user types the answer."
                        },
                        "options": {
                            "type": "array",
                            "items": { "type": "string" },
                            "description": "2-4 mutually exclusive choices for single or multi, one short line each."
                        },
                        "allow_other": {
                            "type": "boolean",
                            "description": "Adds a free-text entry after the options."
                        },
                        "min_choices": {
                            "type": "integer",
                            "description": "multi only: fewest picks the user must make (default 0)."
                        },
                        "max_choices": {
                            "type": "integer",
                            "description": "multi only: most picks the user may make (default: all options)."
                        }
                    },
                    "required": ["question", "kind"]
                }
            }
        },
        "required": []
    })
}

fn user_input_description() -> String {
    format!(
        "Ask the user a question and wait for the answer (`{REQUEST_USER_INPUT_TOOL}`). \
         The turn pauses until the user responds or the request is cancelled. \
         Assistant prose does not pause the turn.\n\n\
         Single question: `question`, plus optional `options` (2–4 mutually \
         exclusive choices, one short line each). Omit `options` for a free-text \
         answer such as a credential, a name, or a path.\n\n\
         Several questions in one interaction: `questions`. Each item requires \
         `question` and `kind` (`single`, `multi`, or `text`). `single` and \
         `multi` take `options`. Optional fields: `header` (short tab label), \
         `allow_other`, `min_choices`, `max_choices`. Optional `question` on the \
         call is the interaction's one-line headline."
    )
}

/// Whether `name` is a clarification tool (primary or legacy).
pub(crate) fn is_user_input_tool(name: &str) -> bool {
    name == REQUEST_USER_INPUT_TOOL || name == ASK_USER_TOOL
}

/// Primary clarification tool definition.
///
/// The legacy `ask_user` name is a parser alias only
/// ([`is_user_input_tool`]): the model-visible surface never advertises it, so
/// no definition is built for it.
pub(crate) fn request_user_input_tool_definition() -> ToolDefinition {
    ToolDefinition {
        name: REQUEST_USER_INPUT_TOOL.to_string(),
        description: user_input_description(),
        input_schema: user_input_input_schema(),
    }
}

/// The name of the injected goal-resolution tool (goal mode only). The run does
/// not end when the model goes quiet — it ends only when the model calls this to
/// mark the objective `complete` (done and truthfully reported) or `blocked`
/// (truly stuck).
pub(crate) const UPDATE_GOAL_TOOL: &str = "update_goal";

/// Resolve the current objective. Verification is evidence, not a universal
/// completion gate: the description asks for a truthful claim about the
/// current workspace state, not for a named proof tool. Completion semantics,
/// the objective-as-stated rule, and the tool's shape are unchanged. A plan is
/// a declaration the model may update freely; the description does not require
/// a terminal Plan refresh.
pub(crate) fn update_goal_tool_definition() -> ToolDefinition {
    let description = String::from(
        "Resolve the current objective. Call this ONLY to end the \
            task: `complete` when every requirement is done, against the \
            current workspace state; \
            `blocked` when you are genuinely and \
            repeatedly stuck and cannot make progress. Going silent does NOT end \
            the task — you must call this. Do not mark complete on unproven or \
            indirect evidence, and do not redefine success down to what already \
            exists. `complete` means the objective AS THE USER STATED IT. If the \
            stated objective cannot be satisfied — it conflicts with something \
            you must not change (an existing test that pins the opposite \
            behavior, a frozen interface, an explicit prohibition) — do NOT \
            reinterpret or narrow its terms into a weaker task and complete \
            that instead: that is a false completion. Report `blocked`, name \
            the exact conflict (which requirement collides with which \
            constraint), and first revert edits that served only the abandoned \
            attempt so the workspace is left clean. Partial conflicts are the \
            same rule: when only PART of the stated objective is satisfiable, \
            delivering that part while quietly exempting the rest — with or \
            without a rationalization — is still a false completion. Blocked, \
            naming which part cannot be satisfied and why.",
    );
    ToolDefinition {
        name: UPDATE_GOAL_TOOL.to_string(),
        description,
        input_schema: serde_json::json!({
            "type": "object",
            "properties": {
                "status": {
                    "type": "string",
                    "enum": ["complete", "blocked"],
                    "description": "complete = done, for the objective as stated; blocked = truly stuck, or the stated objective cannot be satisfied as written (say what conflicts with what)."
                },
                "summary": {
                    "type": "string",
                    "description": "Concise goal summary shown by the TUI as a recap. Keep ≤12 words for complete; longer only when blocked. Do not restate the user question or list files you read."
                },
                "next_step": {
                    "type": "string",
                    "description": "Optional concrete follow-up for the user. Omit this field when no genuine next step remains; never copy or paraphrase the conversation merely to fill it."
                }
            },
            "required": ["status", "summary"]
        }),
    }
}

/// The name of the injected request-permissions tool.
pub(crate) const REQUEST_PERMISSIONS_TOOL: &str = "request_permissions";

/// The name of the injected sub-agent spawn tool.
pub(crate) const SPAWN_AGENT_TOOL: &str = "spawn_agent";

/// The tool the model calls to run a focused sub-agent on a self-contained
/// subtask, getting back only its final result. Emitting several calls in one
/// turn runs the sub-agents CONCURRENTLY.
pub(crate) fn spawn_agent_tool_definition() -> ToolDefinition {
    ToolDefinition {
        name: SPAWN_AGENT_TOOL.to_string(),
        description: "Start a child agent on a subtask. The child uses this model \
            and workspace and starts a new conversation: it does not see this one. \
            `task` is the child's whole instruction. `title` is the short display \
            name and stays fixed for that child's lifetime.\n\n\
            Calls in one assistant message run concurrently. Calls in a later \
            message run after those return. `run_in_background` defaults to true: \
            the call returns the child's id immediately, and the runtime reports \
            the child's result, including partial work, when the child settles. \
            false waits for that result before this call returns.\n\n\
            The child can read immediately. A write requires claim_write_scope. \
            The ownership fence denies overlapping claims and refuses this \
            agent's writes, by editor or by shell, to a path a running child owns.\n\n\
            `agent` names a declarative agent (project `.leveler/agents/<name>/`, \
            user-level, or built-in). That definition fixes capability, tools, \
            write bounds, model, and instructions. A different `role` or `profile` \
            on the same call is refused. `profile` and `role` are aliases: \
            explorer is read-only, worker is a scoped writer and requires `files`, \
            default claims its own write scope. Reviewer cannot be requested. An \
            unknown agent or role is refused."
            .to_string(),
        input_schema: serde_json::json!({
            "type": "object",
            "properties": {
                "title": {
                    "type": "string",
                    "description": "Short display name for the delegated task, roughly 3–8 words. Not a copy of `task`. Fixed for the child's lifetime."
                },
                "task": { "type": "string", "description": "The complete, self-contained instruction for the sub-agent." },
                "profile": {
                    "type": "string",
                    "enum": ["default", "explorer", "worker"],
                    "description": "Optional capability contract: explorer = read-only investigation; worker = scoped writer (requires `files`); default = claims its own write scope. Omit for the default child. Reviewer is harness-launched and cannot be requested here. Alias of `role` when both agree; conflicting values are refused."
                },
                "role": {
                    "type": "string",
                    "enum": ["default", "explorer", "worker"],
                    "description": "Optional. Historical alias of `profile`. explorer = read-only investigation; worker = legacy pre-scoped writer (requires `files`); default = normal child that claims its own write scope."
                },
                "files": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description": "Legacy, for role='worker' only: pre-claimed exclusive paths. Normal children omit this and claim their own scope after reading the code."
                },
                "agent": {
                    "type": "string",
                    "description": "Name of an available agent (project `.leveler/agents/<name>/`, user-level, or built-in). Its definition sets the capability; an unknown or invalid name is refused."
                },
                "run_in_background": {
                    "type": "boolean",
                    "description": "Defaults to true: the call returns the child's id immediately and the runtime reports settlement later. false: this call waits for the child's result."
                }
            },
            "required": ["task", "title"]
        }),
    }
}

/// The tool a CHILD calls to acquire exclusive write authority over the paths
/// it actually needs, after reading enough code to know them (late-bound
/// ownership: SPAWN != WRITE AUTHORITY).
pub(crate) const CLAIM_WRITE_SCOPE_TOOL: &str = "claim_write_scope";

pub(crate) fn claim_write_scope_tool_definition() -> ToolDefinition {
    ToolDefinition {
        name: CLAIM_WRITE_SCOPE_TOOL.to_string(),
        description: "Claim exclusive write authority over the listed files or \
            directories. Before the first successful claim, mutations are refused. \
            The grant is exclusive and atomic: every path or none. An overlapping \
            claim is denied while another child holds it. The grant is released \
            when this child finishes. A denial does not change the workspace. A \
            later call can add paths."
            .to_string(),
        input_schema: serde_json::json!({
            "type": "object",
            "properties": {
                "paths": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description": "Relative files or directories to own exclusively (a directory covers its whole subtree, e.g. 'src/output/'). Claim the smallest scope that covers your work — never the repository root."
                }
            },
            "required": ["paths"]
        }),
    }
}

/// Runtime default resolution for `run_in_background`: an omitted parameter
/// means background. The model relies on the advertised default; it does not
/// have to reproduce it on every call.
pub(crate) fn resolve_run_in_background(arguments: &serde_json::Value) -> bool {
    arguments
        .get("run_in_background")
        .and_then(|v| v.as_bool())
        .unwrap_or(true)
}

/// The tool a CHILD calls to report one typed finding as it is confirmed.
pub(crate) const REPORT_FINDING_TOOL: &str = "report_finding";

pub(crate) fn report_finding_tool_definition() -> ToolDefinition {
    ToolDefinition {
        name: REPORT_FINDING_TOOL.to_string(),
        description: "Record one finding. Each call stores one finding and the \
            parent receives the stored findings with this child's result, including \
            when the run stops early.\n\n\
            Arguments: `kind` (required: relevant_file, relevant_symbol, \
            dependency, callsite, risk, test, config, observation, correctness), \
            `summary` (required text), optional `file`, optional `symbol`."
            .to_string(),
        input_schema: serde_json::json!({
            "type": "object",
            "properties": {
                "kind": {
                    "type": "string",
                    "enum": [
                        "relevant_file", "relevant_symbol", "dependency",
                        "callsite", "risk", "test", "config", "observation",
                        "correctness"
                    ],
                    "description": "What kind of thing this finding is."
                },
                "summary": {
                    "type": "string",
                    "description": "One specific, evidence-backed sentence."
                },
                "file": { "type": "string", "description": "The file it concerns, if any." },
                "symbol": { "type": "string", "description": "The symbol it concerns, if any." }
            },
            "required": ["kind", "summary"]
        }),
    }
}

/// Turn-scoped elevations from an approved `request_permissions` call.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct TurnPermissionGrants {
    pub network: bool,
    /// Allow writes to the current workspace's Git metadata while preserving
    /// the workspace boundary.
    pub repository_git: bool,
    /// Drop OS write confinement for run_command/shell_command this turn.
    pub unrestricted_fs: bool,
}

impl TurnPermissionGrants {
    pub fn is_empty(self) -> bool {
        !self.network && !self.repository_git && !self.unrestricted_fs
    }

    /// Whether everything `requested` asks for is already granted.
    pub fn covers(self, requested: Self) -> bool {
        (self.network || !requested.network)
            && (self.repository_git || self.unrestricted_fs || !requested.repository_git)
            && (self.unrestricted_fs || !requested.unrestricted_fs)
    }

    pub fn merge(self, other: Self) -> Self {
        Self {
            network: self.network || other.network,
            repository_git: self.repository_git || other.repository_git,
            unrestricted_fs: self.unrestricted_fs || other.unrestricted_fs,
        }
    }
}

/// Parse model arguments for `request_permissions` (current fields plus legacy aliases).
///
/// - `network` (bool): request network for the rest of the turn
/// - `filesystem`: `"workspace"` (default) | `"git"` | `"unrestricted"` — Git
///   permits current-repository metadata writes; unrestricted clears confinement
/// - `full_access` (bool): shorthand for network + unrestricted filesystem
/// - If none of the above are set, defaults to **network only** (legacy behavior)
pub(crate) fn parse_permission_request(
    args: &serde_json::Value,
) -> (String, String, TurnPermissionGrants) {
    let action = args
        .get("action")
        .and_then(|v| v.as_str())
        .unwrap_or("(未说明)")
        .to_string();
    let reason = args
        .get("reason")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();

    let (mut grants, named) = parse_grant_axes(args);
    if !named {
        // Legacy: only action/reason → network elevation.
        grants.network = true;
    }
    (action, reason, grants)
}

/// The command tools whose calls may carry an `escalate` retry.
const ESCALATABLE_TOOLS: [&str; 2] = ["shell_command", "run_command"];

/// Read the grant axes (`network` / `filesystem` / `full_access`) shared by
/// `request_permissions` and a command's `escalate`. Returns the grants plus
/// whether any axis was actually named — the two callers disagree on what an
/// unnamed axis means, so that judgement stays with them.
fn parse_grant_axes(args: &serde_json::Value) -> (TurnPermissionGrants, bool) {
    let full_access = args
        .get("full_access")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let network = args.get("network").and_then(|v| v.as_bool());
    let filesystem = args
        .get("filesystem")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    if full_access {
        return (
            TurnPermissionGrants {
                network: true,
                repository_git: false,
                unrestricted_fs: true,
            },
            true,
        );
    }
    let named = network.is_some() || !filesystem.is_empty();
    (
        TurnPermissionGrants {
            network: network.unwrap_or(false),
            repository_git: filesystem == "git",
            unrestricted_fs: matches!(
                filesystem,
                "unrestricted" | "full" | "full_access" | "danger-full-access"
            ),
        },
        named,
    )
}

/// Whether a tool's calls may carry `escalate`.
pub(crate) fn is_escalatable_tool(name: &str) -> bool {
    ESCALATABLE_TOOLS.contains(&name)
}

/// Parse a command call's `escalate` retry into its reason and grants.
///
/// `None` means the call is an ordinary one. Unlike `request_permissions`,
/// an `escalate` naming no axis yields EMPTY grants rather than the legacy
/// network default: this argument exists to answer a denial the model just
/// saw, so it must name the axis that denial was about. The caller turns
/// empty grants into a refusal.
pub(crate) fn parse_escalation(args: &serde_json::Value) -> Option<(String, TurnPermissionGrants)> {
    let escalate = args.get("escalate")?;
    if !escalate.is_object() {
        return None;
    }
    let reason = escalate
        .get("reason")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let (grants, _) = parse_grant_axes(escalate);
    Some((reason, grants))
}

/// The command line an `escalate` prompt shows the user. The command IS the
/// action here — unlike `request_permissions`, the model does not restate it.
pub(crate) fn escalation_action(call: &ToolCall) -> String {
    if let Some(cmd) = call.arguments.get("cmd").and_then(|v| v.as_str()) {
        return cmd.to_string();
    }
    let program = call
        .arguments
        .get("program")
        .and_then(|v| v.as_str())
        .unwrap_or(call.name.as_str());
    let args: Vec<&str> = call
        .arguments
        .get("args")
        .and_then(|v| v.as_array())
        .map(|args| args.iter().filter_map(|arg| arg.as_str()).collect())
        .unwrap_or_default();
    format!("{program} {}", args.join(" ")).trim().to_string()
}

/// An `escalate` that names no axis. Refused without a prompt: there is
/// nothing concrete to ask the user to approve.
pub(crate) fn escalation_missing_axis_message() -> String {
    "escalate named no permission: set `network: true`, `filesystem: \"git\"` or `filesystem: \"unrestricted\"`, \
     or `full_access: true` on it, matching the access the sandbox actually denied. \
     The command was NOT run."
        .to_string()
}

/// The `escalate` property added to each command tool's published schema.
fn escalation_property() -> serde_json::Value {
    serde_json::json!({
        "type": "object",
        "description": "Retry this EXACT command with elevated permission after the \
            sandbox denied it. The approval prompt this raises is how the user consents \
            — do not ask in prose first, and do not call request_permissions for it. \
            Ground it in a denial you just saw: never escalate speculatively. Ask only for \
            what that denial showed: a network failure needs `network`, not `filesystem`. \
            The user chooses whether the grant covers this call or the rest of the turn. \
            If the user denies, that answer is final.",
        "properties": {
            "reason": {
                "type": "string",
                "description": "One sentence for the user: why this exact command needs the wider access."
            },
            "network": {
                "type": "boolean",
                "description": "This call needs network access."
            },
            "filesystem": {
                "type": "string",
                "enum": ["workspace", "git", "unrestricted"],
                "description": "git = allow current-repository Git metadata writes while keeping workspace confinement; unrestricted = drop workspace confinement. Request the narrowest axis matching the observed denial."
            },
            "full_access": {
                "type": "boolean",
                "description": "Shorthand for network=true and filesystem=unrestricted."
            }
        },
        "required": ["reason"]
    })
}

/// Publish `escalate` on the command tools.
///
/// Mounted only while the profile actually confines something — under
/// 完全访问 there is nothing to escalate to, so the argument would only invite
/// a pointless prompt (same reason `request_permissions` is withheld there).
pub(crate) fn advertise_escalation(tools: &mut [ToolDefinition]) {
    for tool in tools
        .iter_mut()
        .filter(|tool| is_escalatable_tool(&tool.name))
    {
        if let Some(properties) = tool
            .input_schema
            .get_mut("properties")
            .and_then(|p| p.as_object_mut())
        {
            properties.insert("escalate".to_string(), escalation_property());
        }
    }
}

/// How long an approved elevation lasts. `request_permissions` holds for the
/// rest of the turn, and its prompt says so. A command's `escalate` lasts as
/// long as the option the user picks — this call ("仅允许本次") or the turn
/// ("本轮对话内允许") — so its description claims neither.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum GrantScope {
    Turn,
    SingleCall,
}

impl GrantScope {
    fn fs_note(self) -> &'static str {
        match self {
            Self::Turn => "本轮写不受工作区沙箱限制",
            Self::SingleCall => "写不受工作区沙箱限制",
        }
    }

    fn prefix(self) -> &'static str {
        match self {
            Self::Turn => "本轮需要",
            Self::SingleCall => "需要",
        }
    }
}

/// What the user reads at the moment they decide.
///
/// It used to glue three machine fields with colons and nest them in
/// parentheses — "命令请求网络:curl -sS …(原因:用户要求…)" — inside the
/// parentheses the client adds of its own. One sentence, one separator.
pub(crate) fn permission_request_description(
    action: &str,
    reason: &str,
    grants: TurnPermissionGrants,
    scope: GrantScope,
) -> String {
    let mut parts = Vec::new();
    if grants.network {
        parts.push("网络");
    }
    if grants.repository_git {
        parts.push("当前仓库 Git 元数据写入");
    }
    if grants.unrestricted_fs {
        parts.push(scope.fs_note());
    }
    let what = if parts.is_empty() {
        "权限".to_string()
    } else {
        parts.join(" + ")
    };
    let prefix = scope.prefix();
    let mut text = format!("{prefix}{what}");
    if !action.trim().is_empty() {
        text.push_str(" · ");
        text.push_str(action.trim());
    }
    if !reason.trim().is_empty() {
        text.push_str(" · 原因：");
        text.push_str(reason.trim());
    }
    text
}

pub(crate) fn permission_grant_message(granted: bool, grants: TurnPermissionGrants) -> String {
    if granted {
        permission_granted_message(grants)
    } else {
        permission_denied_by_user_message()
    }
}

fn permission_granted_message(grants: TurnPermissionGrants) -> String {
    let mut parts = Vec::new();
    if grants.network {
        parts.push("网络访问");
    }
    if grants.repository_git {
        parts.push("当前仓库 Git 元数据写入");
    }
    if grants.unrestricted_fs {
        parts.push("本轮无限制写(命令不再套工作区写沙箱)");
    }
    if parts.is_empty() {
        "已获授权。".to_string()
    } else {
        format!("已获授权:本轮允许{}。", parts.join("、"))
    }
}

/// Model-facing result when a person clicked Deny. English is protocol
/// guidance for the model, not TUI copy.
pub(crate) fn permission_denied_by_user_message() -> String {
    "The user explicitly denied this permission request. \
     Continue only with capabilities already available. \
     Do not request the same or broader permission again for this task \
     unless the user explicitly changes their decision. \
     If you now need the user to perform an action or provide information, \
     use request_user_input. \
     If the task cannot proceed without the denied capability, \
     report the task as blocked rather than repeatedly retrying."
        .to_string()
}

pub(crate) fn permission_already_denied_message() -> String {
    "The user already denied this permission for the current task. \
     Do not request it again. Continue only with capabilities already available, \
     or use request_user_input if you need the user to act."
        .to_string()
}

pub(crate) fn permission_denied_unattended_message() -> String {
    "Permission was not granted (no human was available to approve). \
     Continue only with capabilities already available. \
     Do not treat this as a user refusal."
        .to_string()
}

/// Outcome of `request_permissions`. Human Deny is not the same fact as
/// a headless auto-deny.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum PermissionRequestOutcome {
    Granted {
        grants: TurnPermissionGrants,
        /// The user chose to allow it for the rest of this turn, not just
        /// what was asked. Only a single-call escalation reads it; a
        /// `request_permissions` grant already lasts the turn.
        for_turn: bool,
        message: String,
    },
    DeniedByUser {
        requested: TurnPermissionGrants,
        message: String,
    },
    DeniedUnattended {
        requested: TurnPermissionGrants,
        message: String,
    },
    Invalid {
        message: String,
    },
}

impl PermissionRequestOutcome {
    pub(crate) fn message(&self) -> &str {
        match self {
            Self::Granted { message, .. }
            | Self::DeniedByUser { message, .. }
            | Self::DeniedUnattended { message, .. }
            | Self::Invalid { message } => message,
        }
    }

    pub(crate) fn is_error(&self) -> bool {
        !matches!(self, Self::Granted { .. })
    }
}

/// Apply turn grants onto a tool context (network + optional unrestricted FS).
pub(crate) fn apply_turn_grants(
    mut ctx: leveler_tools::ToolContext,
    grants: TurnPermissionGrants,
) -> leveler_tools::ToolContext {
    if grants.network {
        ctx.policy.grant_network();
    }
    if grants.repository_git {
        ctx.policy.grant_repository_git();
    }
    if grants.unrestricted_fs {
        ctx.policy.grant_unrestricted_fs();
    }
    ctx
}

/// The tool the model calls to ask for elevated permission (network / filesystem).
pub(crate) fn request_permissions_tool_definition() -> ToolDefinition {
    ToolDefinition {
        name: REQUEST_PERMISSIONS_TOOL.to_string(),
        description: "Ask for a permission grant that lasts the rest of this turn. \
            Arguments: `action` (required), optional `reason`, `network` (bool), \
            `filesystem` (`workspace`, `git`, or `unrestricted`), `full_access` \
            (bool: network plus unrestricted filesystem). A call that names none \
            of those three requests network only. `git` allows current-repository \
            Git metadata writes. `unrestricted` drops command write confinement. \
            Approval applies the named grant until the turn ends. Denial grants \
            nothing. This tool is not offered when the mode is already full access."
            .to_string(),
        input_schema: serde_json::json!({
            "type": "object",
            "properties": {
                "action": { "type": "string", "description": "What you want to do that needs permission." },
                "reason": { "type": "string", "description": "Why it is necessary." },
                "network": { "type": "boolean", "description": "Request network access for this turn." },
                "filesystem": {
                    "type": "string",
                    "enum": ["workspace", "git", "unrestricted"],
                    "description": "workspace = keep write sandbox (default); git = allow current-repository Git metadata writes; unrestricted = no write confinement for commands this turn."
                },
                "full_access": {
                    "type": "boolean",
                    "description": "Shorthand for network=true and filesystem=unrestricted."
                }
            },
            "required": ["action"]
        }),
    }
}

/// Agent plan synchronization contract. The plan is the user's live view of
/// the work and the seed a resumed turn reads. The runtime records and shows it
/// but never enforces it: it does not infer statuses, does not require a
/// terminal refresh, and does not require every step to be completed.
#[cfg(test)]
mod plan_sync_contract_tests {
    const BASE_PROMPT: &str = include_str!("../prompts/base.md");

    fn plan_section() -> &'static str {
        let start = BASE_PROMPT
            .find("## Plan")
            .expect("base prompt must carry a plan section");
        let rest = &BASE_PROMPT[start + 2..];
        &BASE_PROMPT[start..start + 2 + rest.find("\n## ").unwrap_or(rest.len())]
    }

    #[test]
    fn base_prompt_says_the_plan_is_a_declaration() {
        let section = plan_section();
        for needle in [
            "update_plan",
            "declared intent",
            "not verification evidence",
        ] {
            assert!(
                section.contains(needle),
                "plan section lost `{needle}`:\n{section}"
            );
        }
    }

    /// A completed step is an observed outcome. The prompt does not script
    /// when to retry or how to rewrite the list.
    #[test]
    fn completed_means_an_observed_outcome() {
        let section = plan_section();
        assert!(
            section.contains("Only observed outcomes justify `completed`"),
            "COMPLETE lost:\n{section}"
        );
    }

    #[test]
    fn a_failed_action_is_not_reported_completed() {
        let section = plan_section();
        assert!(
            section.contains(
                "failed, denied, abandoned or unfinished work must not be reported as completed"
            ),
            "FAILURE lost:\n{section}"
        );
        assert!(
            !section.contains("stays `in_progress`"),
            "the plan contract must not tell the model to retry:\n{section}"
        );
    }

    #[test]
    fn an_abandoned_step_is_not_marked_completed() {
        let section = plan_section();
        assert!(
            section.contains("abandoned"),
            "abandoned work must not be reported completed:\n{section}"
        );
        assert!(
            !section.contains("rewrite that step"),
            "rewriting the plan is the model's decision:\n{section}"
        );
    }

    /// Timing rules for when the plan must move are strategy.
    #[test]
    fn the_plan_section_does_not_schedule_updates() {
        let section = plan_section();
        for banned in [
            "may trail",
            "already left behind",
            "Converge:",
            "- Close:",
            "before `update_goal`",
            "same response as the first tool call",
        ] {
            assert!(
                !section.contains(banned),
                "plan section must not schedule updates (`{banned}`):\n{section}"
            );
        }
    }

    /// Terminal reconciliation is freshness, never an instruction to turn all
    /// rows green. Both ordinary closeout and goal closeout state the same
    /// contract where the model reads it.
    #[test]
    fn plan_completion_gate_is_removed_but_plan_truth_remains() {
        let section = plan_section();
        assert!(
            !section.contains("- Close:") && !section.contains("before `update_goal`"),
            "the mandatory terminal Plan ceremony must be gone:\n{section}"
        );
        assert!(
            section.contains("Only observed outcomes justify `completed`")
                && section.contains("must not be reported as completed"),
            "plan truthfulness rules must remain:\n{section}"
        );
        let def = super::update_goal_tool_definition();
        assert!(
            !def.description.contains("update_plan"),
            "the goal tool must not mandate a terminal Plan refresh: {}",
            def.description
        );
        for needle in ["AS THE USER STATED IT", "false completion", "`blocked`"] {
            assert!(
                def.description.contains(needle),
                "completion semantics lost `{needle}`"
            );
        }
    }

    /// The plan is the agent's declared progress. The runtime records, persists
    /// and shows it; it does not observe whether the work is really at the
    /// declared step. The declaration framing lives in the prompt section the
    /// model reads; the tool contract states that the runtime advances nothing
    /// on its own. Neither text may present the plan as the live state of the
    /// work.
    #[test]
    fn the_plan_is_described_as_declared_progress() {
        let def = crate::update_plan::UpdatePlanTool;
        let description = leveler_tools::Tool::description(&def);
        let section = plan_section();
        assert!(
            section.contains("declared intent"),
            "plan section must frame the plan as a declaration:\n{section}"
        );
        assert!(
            description.contains("nothing else advances"),
            "the tool contract says the runtime advances nothing:\n{description}"
        );
        for text in [section, description] {
            assert!(
                !text.contains("state of the work"),
                "plan must not be framed as the live state of the work:\n{text}"
            );
        }
    }

    /// Order is the model's decision: the contract records the list it is sent
    /// and makes no later step conditional on an earlier one.
    #[test]
    fn the_plan_contract_imposes_no_order_rule() {
        let def = crate::update_plan::UpdatePlanTool;
        let description = leveler_tools::Tool::description(&def);
        for banned in [
            "intended order",
            "completed in order",
            "sequentially",
            "one at a time",
        ] {
            assert!(
                !description.contains(banned),
                "update_plan imposes an order rule (`{banned}`):\n{description}"
            );
        }
    }

    #[test]
    fn plan_sync_is_described_without_a_completion_mandate() {
        let section = plan_section();
        assert!(
            !section.contains("freshness requirement")
                && !section.contains("Close:")
                && !section.contains("before `update_goal`"),
            "the terminal Plan mandate must be gone:\n{section}"
        );
        assert!(
            section.contains("declared intent and status"),
            "the plan stays a declaration:\n{section}"
        );
    }
}

#[cfg(test)]
mod scope_fidelity_tests {
    /// ICG-6R: the completion contract must keep saying, in so many words,
    /// that an unsatisfiable objective is `blocked` — never reinterpreted into
    /// a weaker task and completed. A wording regression here re-opens the
    /// scope-substitution false-completion path.
    #[test]
    fn update_goal_contract_forbids_scope_substitution() {
        let def = super::update_goal_tool_definition();
        for needle in [
            "AS THE USER STATED IT",
            "Partial conflicts",
            "do NOT",
            "false completion",
            "revert edits",
        ] {
            assert!(
                def.description.contains(needle),
                "completion contract lost `{needle}`"
            );
        }
    }

    /// Verification productionized after the three-arm experiment: the
    /// description demands a truthful claim about the current workspace state
    /// and nothing more. A mandatory proof tool (or a last-edit proof rule)
    /// here would be a completion gate the harness deliberately does not have.
    #[test]
    fn update_goal_does_not_mandate_a_proof_tool() {
        let def = super::update_goal_tool_definition();
        for banned in [
            "PROVEN",
            "since your last edit",
            "build/tests",
            "build and tests",
        ] {
            assert!(
                !def.description.contains(banned),
                "verification must stay evidence, not a completion gate: `{banned}`"
            );
        }
        for needle in [
            "AS THE USER STATED IT",
            "false completion",
            "`blocked`",
            "against the current workspace state",
        ] {
            assert!(
                def.description.contains(needle),
                "completion semantics lost `{needle}`"
            );
        }
    }

    /// With production semantics finalized, there is no Plan close-coaching
    /// seam left to ablate: the tool description never mandates a terminal
    /// Plan refresh and always keeps the completion contract.
    #[test]
    fn update_goal_never_mandates_a_terminal_plan_refresh() {
        let def = super::update_goal_tool_definition();
        for banned in [
            "first call update_plan",
            "You may call update_plan then update_goal",
            "unfinished steps stay pending",
            "PROVEN",
        ] {
            assert!(
                !def.description.contains(banned),
                "production must not carry `{banned}`: {}",
                def.description
            );
        }
        for needle in ["AS THE USER STATED IT", "false completion", "`blocked`"] {
            assert!(
                def.description.contains(needle),
                "completion contract lost `{needle}`"
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn worker_scope_advertises_directory_grants() {
        // Ergonomics (MA-WA1): at decision time Main often knows the module,
        // not every file. Directory scope has always been enforced (allowlist
        // + overlap both use directory-prefix semantics) — the schema must say
        // so, or the model believes it needs perfect file knowledge to spawn.
        let def = spawn_agent_tool_definition();
        let files_desc = def.input_schema["properties"]["files"]["description"]
            .as_str()
            .unwrap();
        assert!(
            files_desc.contains("Legacy") && files_desc.contains("claim"),
            "files must read as the legacy path, pointing at claim_write_scope: {files_desc}"
        );
        // Late-bound ownership: the claim tool advertises directory grants.
        let claim = claim_write_scope_tool_definition();
        let claim_desc = claim.input_schema["properties"]["paths"]["description"]
            .as_str()
            .unwrap();
        assert!(
            claim_desc.contains("director"),
            "claim paths must advertise directory grants: {claim_desc}"
        );
    }

    /// V2 four-point background contract, tool-description + parameter points:
    /// the advertised default is background, the call returns immediately with
    /// the child's id, settlement is promised, and the foreground escape is
    /// scoped to "next action depends on it". Runtime point is
    /// `resolve_run_in_background`; prompt point is the steer hint (tested in
    /// sub_agent.rs).
    #[test]
    fn spawn_agent_advertises_the_background_first_contract() {
        let def = spawn_agent_tool_definition();
        let d = def.description.to_ascii_lowercase();
        assert!(d.contains("defaults to true"), "{}", def.description);
        assert!(
            d.contains("returns the child's id immediately") || d.contains("returns immediately"),
            "{}",
            def.description
        );
        assert!(d.contains("settle"), "{}", def.description);
        assert!(
            d.contains("claim_write_scope") || (d.contains("claim") && d.contains("write")),
            "the late-bound claim protocol must be model-visible: {}",
            def.description
        );
        assert!(
            !d.contains("keep the work yourself"),
            "delegation choice is not a tool instruction: {}",
            def.description
        );
        assert!(
            !d.contains("when the user asks for parallel/multi-agent work"),
            "{}",
            def.description
        );
        assert!(!d.contains("always spawn"), "{}", def.description);

        let param = def.input_schema["properties"]["run_in_background"]["description"]
            .as_str()
            .unwrap()
            .to_ascii_lowercase();
        assert!(param.contains("defaults to true"), "{param}");
        assert!(param.contains("waits for the child's result"), "{param}");
        assert!(!param.contains("next action depends"), "{param}");
    }

    /// The child's short task title is spawn-time identity, separate from the
    /// full `task` instructions. The schema has to require it and say how it
    /// differs, or the UI has only the instructions to show.
    #[test]
    fn spawn_agent_requires_a_separate_short_task_title() {
        let def = spawn_agent_tool_definition();
        assert_eq!(
            def.input_schema["required"],
            serde_json::json!(["task", "title"]),
            "title is part of the spawn contract"
        );
        let title = def.input_schema["properties"]["title"]["description"]
            .as_str()
            .unwrap();
        assert!(
            title.to_ascii_lowercase().contains("not a copy of `task`"),
            "{title}"
        );
        assert!(title.contains("3–8 words"), "{title}");
        assert!(
            def.description.contains("short display name"),
            "{}",
            def.description
        );
    }

    #[test]
    fn run_in_background_default_is_runtime_resolved_not_model_remembered() {
        assert!(resolve_run_in_background(&serde_json::json!({"task": "x"})));
        assert!(resolve_run_in_background(
            &serde_json::json!({"task": "x", "run_in_background": true})
        ));
        assert!(!resolve_run_in_background(
            &serde_json::json!({"task": "x", "run_in_background": false})
        ));
        // Malformed value degrades to the advertised default, never to a
        // silent foreground surprise.
        assert!(resolve_run_in_background(
            &serde_json::json!({"task": "x", "run_in_background": "yes"})
        ));
    }

    #[test]
    fn legacy_action_only_requests_network() {
        let (_, _, g) = parse_permission_request(&serde_json::json!({"action": "curl"}));
        assert_eq!(
            g,
            TurnPermissionGrants {
                network: true,
                repository_git: false,
                unrestricted_fs: false
            }
        );
    }

    #[test]
    fn full_access_shorthand_sets_both() {
        let (_, _, g) =
            parse_permission_request(&serde_json::json!({"action": "x", "full_access": true}));
        assert!(g.network && g.unrestricted_fs);
    }

    #[test]
    fn filesystem_unrestricted_without_network() {
        let (_, _, g) = parse_permission_request(&serde_json::json!({
            "action": "write outside",
            "network": false,
            "filesystem": "unrestricted"
        }));
        assert!(!g.network && g.unrestricted_fs);
    }

    #[test]
    fn repository_git_permission_is_narrower_than_unrestricted_filesystem() {
        let (_, _, g) = parse_permission_request(&serde_json::json!({
            "action": "git commit",
            "filesystem": "git"
        }));
        assert!(!g.network);
        assert!(g.repository_git);
        assert!(!g.unrestricted_fs);

        let unrestricted = TurnPermissionGrants {
            network: false,
            repository_git: false,
            unrestricted_fs: true,
        };
        assert!(unrestricted.covers(g));
    }

    #[test]
    fn repository_git_grant_keeps_workspace_confinement() {
        let dir = tempfile::tempdir().unwrap();
        let ws = leveler_execution::Workspace::new(dir.path()).unwrap();
        let ctx =
            leveler_tools::ToolContext::new(ws, leveler_execution::PermissionProfile::Assisted);
        let elevated = apply_turn_grants(
            ctx,
            TurnPermissionGrants {
                network: false,
                repository_git: true,
                unrestricted_fs: false,
            },
        );
        assert_eq!(
            elevated.write_scope(),
            leveler_execution::WriteScope::WorkspaceWithGit {
                root: dir.path().canonicalize().unwrap()
            }
        );
    }

    #[test]
    fn apply_turn_grants_sets_network_and_fs_flags() {
        let dir = std::env::temp_dir().join(format!(
            "leveler-grant-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let ws = leveler_execution::Workspace::new(&dir).unwrap();
        let ctx =
            leveler_tools::ToolContext::new(ws, leveler_execution::PermissionProfile::Assisted)
                .with_sandbox(true);
        assert!(ctx.policy.network_denied());
        assert!(!ctx.policy.unrestricted_fs());
        let elevated = apply_turn_grants(
            ctx,
            TurnPermissionGrants {
                network: true,
                repository_git: false,
                unrestricted_fs: true,
            },
        );
        assert!(!elevated.policy.network_denied());
        assert!(elevated.policy.unrestricted_fs());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn human_denial_message_is_not_a_bare_false() {
        let msg = permission_denied_by_user_message();
        assert!(msg.contains("explicitly denied"));
        assert!(msg.contains("request_user_input"));
        assert!(!msg.contains("用户未批准"));
        let unattended = permission_denied_unattended_message();
        assert!(!unattended.contains("explicitly denied"));
        assert!(unattended.contains("no human"));
    }

    #[test]
    fn request_permissions_schema_advertises_filesystem_fields() {
        let tool = request_permissions_tool_definition();
        let props = tool.input_schema["properties"].as_object().unwrap();
        assert!(props.contains_key("network"));
        assert!(props.contains_key("filesystem"));
        assert!(props.contains_key("full_access"));
    }

    #[test]
    fn update_goal_exposes_optional_structured_next_step() {
        let tool = update_goal_tool_definition();
        let properties = tool.input_schema["properties"].as_object().unwrap();
        assert!(properties.contains_key("next_step"));
        assert!(
            !properties.contains_key("override_incomplete_todos"),
            "the plan is not a completion gate, so there is nothing to override"
        );
        let required = tool.input_schema["required"].as_array().unwrap();
        assert!(
            !required.iter().any(|field| field == "next_step"),
            "next_step must be omitted when there is no genuine follow-up"
        );
    }

    #[test]
    fn request_user_input_advertises_multi_question_shape() {
        let tool = request_user_input_tool_definition();
        let questions = &tool.input_schema["properties"]["questions"];
        assert_eq!(questions["type"], "array");
        let item = &questions["items"];
        for field in [
            "header",
            "question",
            "kind",
            "options",
            "allow_other",
            "min_choices",
            "max_choices",
        ] {
            assert!(
                item["properties"].get(field).is_some(),
                "the question schema must carry `{field}`: {item}"
            );
        }
        let kinds = item["properties"]["kind"]["enum"].as_array().unwrap();
        assert_eq!(
            kinds.iter().filter_map(|k| k.as_str()).collect::<Vec<_>>(),
            vec!["single", "multi", "text"]
        );
        // Neither form may be forced on the model: a single-question call
        // omits `questions`, and a multi-question call may omit the headline.
        assert!(
            !tool.input_schema["required"]
                .as_array()
                .unwrap()
                .iter()
                .any(|f| f == "questions" || f == "question"),
            "both shapes must stay optional: {}",
            tool.input_schema
        );
    }

    #[test]
    fn request_user_input_is_primary_with_ask_user_alias() {
        assert!(is_user_input_tool(REQUEST_USER_INPUT_TOOL));
        assert!(is_user_input_tool(ASK_USER_TOOL));
        assert!(!is_user_input_tool("request_permissions"));

        let primary = request_user_input_tool_definition();
        assert_eq!(primary.name, "request_user_input");
        assert!(
            !primary.description.contains("ask_user"),
            "the legacy name is accepted by the parser and is not advertised"
        );
        assert!(primary.input_schema["properties"].get("question").is_some());
        assert!(
            primary.description.contains("mutually exclusive")
                || primary.description.contains("2–4")
                || primary.description.contains("2-4"),
            "tool description must state the choice shape: {}",
            primary.description
        );
        assert!(
            primary.description.contains("does not pause"),
            "prose must not be described as a wait: {}",
            primary.description
        );
    }

    /// The prompt is the user's only view of what they are approving, and it
    /// is read at the moment a security decision is made. It used to be three
    /// machine fields glued with colons and nested parentheses:
    /// "命令请求网络:curl -sS …(原因:用户要求确认…)" — and the client wraps it
    /// in parentheses of its own.
    #[test]
    fn an_escalation_prompt_reads_as_a_sentence() {
        let grants = TurnPermissionGrants {
            network: true,
            repository_git: false,
            unrestricted_fs: false,
        };
        let text = permission_request_description(
            "curl -sS https://registry.npmjs.org/-/ping",
            "确认能否连到 npm registry",
            grants,
            GrantScope::SingleCall,
        );
        assert!(!text.contains('('), "no machine parentheses: {text}");
        assert!(
            !text.contains("网络:") && !text.contains("原因:"),
            "no colon-glued fields: {text}"
        );
        assert!(text.contains("网络"), "{text}");
        assert!(text.contains("curl -sS"), "{text}");
        assert!(text.contains("确认能否连到 npm registry"), "{text}");
    }

    /// The prompt is the user's only view of what they are approving. A
    /// one-call escalation must not be described as lasting the whole turn.
    #[test]
    fn a_single_call_escalation_prompt_does_not_claim_the_turn() {
        let grants = TurnPermissionGrants {
            network: false,
            repository_git: false,
            unrestricted_fs: true,
        };
        let turn = permission_request_description("git pull", "", grants, GrantScope::Turn);
        assert!(turn.contains("本轮"), "{turn}");

        // A command's escalation lasts as long as the option the user picks
        // ("仅允许本次" / "本轮对话内允许"); the description claims neither.
        let once = permission_request_description("git pull", "", grants, GrantScope::SingleCall);
        assert!(!once.contains("本轮"), "{once}");
        assert!(!once.contains("仅此一次"), "{once}");
        assert!(once.contains("写不受工作区沙箱限制"), "{once}");
    }

    #[test]
    fn escalation_is_absent_when_the_call_carries_no_escalate_field() {
        assert!(parse_escalation(&serde_json::json!({"cmd": "git pull"})).is_none());
    }

    #[test]
    fn escalation_parses_the_same_axes_as_request_permissions() {
        let (reason, grants) = parse_escalation(&serde_json::json!({
            "cmd": "git pull --rebase",
            "escalate": { "reason": "remote git needs the network and .git writes", "full_access": true }
        }))
        .expect("escalate must parse");
        assert!(reason.contains("remote git"));
        assert!(grants.network && grants.unrestricted_fs);

        let (_, fs_only) = parse_escalation(&serde_json::json!({
            "escalate": { "reason": "write outside the workspace", "filesystem": "unrestricted" }
        }))
        .unwrap();
        assert!(fs_only.unrestricted_fs && !fs_only.network);
    }

    /// Unlike `request_permissions`, a bare `escalate` must NOT silently mean
    /// network: the model has to name the axis its command was denied on.
    #[test]
    fn an_escalation_naming_no_axis_is_empty_not_a_network_grant() {
        let (_, grants) = parse_escalation(&serde_json::json!({
            "escalate": { "reason": "please" }
        }))
        .unwrap();
        assert!(grants.is_empty(), "a bare escalate must not grant network");
    }

    #[test]
    fn escalation_is_advertised_on_command_tools_only() {
        let mut tools = vec![
            ToolDefinition {
                name: "shell_command".to_string(),
                description: String::new(),
                input_schema: serde_json::json!({"type":"object","properties":{"cmd":{"type":"string"}}}),
            },
            ToolDefinition {
                name: "run_command".to_string(),
                description: String::new(),
                input_schema: serde_json::json!({"type":"object","properties":{"program":{"type":"string"}}}),
            },
            ToolDefinition {
                name: "read_file".to_string(),
                description: String::new(),
                input_schema: serde_json::json!({"type":"object","properties":{"path":{"type":"string"}}}),
            },
        ];
        advertise_escalation(&mut tools);

        for tool in tools.iter().filter(|t| t.name != "read_file") {
            let escalate = tool.input_schema["properties"]
                .get("escalate")
                .unwrap_or_else(|| panic!("{} must advertise escalate", tool.name));
            let props = escalate["properties"].as_object().unwrap();
            for axis in ["reason", "network", "filesystem", "full_access"] {
                assert!(props.contains_key(axis), "{} missing {axis}", tool.name);
            }
            assert_eq!(escalate["required"], serde_json::json!(["reason"]));
        }
        assert!(
            tools[2].input_schema["properties"]
                .get("escalate")
                .is_none(),
            "a read-only tool has nothing to escalate"
        );
        assert!(
            tools[0].input_schema["properties"].get("cmd").is_some(),
            "augmenting the schema must not drop the tool's own fields"
        );
    }
}
