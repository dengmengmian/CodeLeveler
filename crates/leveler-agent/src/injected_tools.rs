//! Definitions of the executor-injected tools (request_user_input / ask_user,
//! update_goal, request_permissions, spawn_agent) advertised to the model.

use leveler_model::ToolDefinition;

/// Primary name for mid-turn clarification.
pub(crate) const REQUEST_USER_INPUT_TOOL: &str = "request_user_input";

/// Legacy alias kept for older prompts / transcripts; same Clarifier path.
pub(crate) const ASK_USER_TOOL: &str = "ask_user";

/// Shared schema for `request_user_input` and `ask_user`.
fn user_input_input_schema() -> serde_json::Value {
    serde_json::json!({
        "type": "object",
        "properties": {
            "question": { "type": "string", "description": "The question to ask." },
            "options": {
                "type": "array",
                "items": { "type": "string" },
                "description": "Optional candidate answers."
            }
        },
        "required": ["question"]
    })
}

fn user_input_description(primary_name: &str, alias: Option<&str>) -> String {
    let alias_note = match alias {
        Some(a) => format!(" (legacy alias: `{a}`)"),
        None => String::new(),
    };
    format!(
        "Ask the user a clarifying question and wait for their answer \
         (`{primary_name}`{alias_note}). Use it at a genuine decision point that is the \
         user's to make: an ambiguous requirement, a choice between viable approaches, \
         overwriting existing work, or a destructive/irreversible action. Prefer asking \
         over guessing at these forks. Do NOT ask about trivial choices you can \
         reasonably make yourself. For approach/scope choice forks, always pass \
         `options` with 2–4 mutually exclusive answers (short label + consequence); \
         put the recommended option first when you have a preference. Do not rely on \
         open-ended chat prose alone (\"想问一下\", \"waiting for confirmation\") — that \
         is a fake pause. Omit `options` only for free-form answers (credentials, \
         names, paths the user must type). Keep `question` to one short sentence."
    )
}

/// Whether `name` is a clarification tool (primary or legacy).
pub(crate) fn is_user_input_tool(name: &str) -> bool {
    name == REQUEST_USER_INPUT_TOOL || name == ASK_USER_TOOL
}

/// Primary clarification tool definition.
pub(crate) fn request_user_input_tool_definition() -> ToolDefinition {
    ToolDefinition {
        name: REQUEST_USER_INPUT_TOOL.to_string(),
        description: user_input_description(REQUEST_USER_INPUT_TOOL, Some(ASK_USER_TOOL)),
        input_schema: user_input_input_schema(),
    }
}

/// Legacy `ask_user` definition (compat; same Clarifier path).
pub(crate) fn ask_user_tool_definition() -> ToolDefinition {
    ToolDefinition {
        name: ASK_USER_TOOL.to_string(),
        description: user_input_description(ASK_USER_TOOL, Some(REQUEST_USER_INPUT_TOOL)),
        input_schema: user_input_input_schema(),
    }
}

/// The name of the injected goal-resolution tool (goal mode only). The run does
/// not end when the model goes quiet — it ends only when the model calls this to
/// mark the objective `complete` (proven, audited) or `blocked` (truly stuck).
pub(crate) const UPDATE_GOAL_TOOL: &str = "update_goal";

pub(crate) fn update_goal_tool_definition() -> ToolDefinition {
    ToolDefinition {
        name: UPDATE_GOAL_TOOL.to_string(),
        description: "Resolve the current objective. Call this ONLY to end the \
            task: `complete` when you have PROVEN, against the current workspace \
            state, that every requirement is done (build/tests run and passed \
            since your last edit); `blocked` when you are genuinely and \
            repeatedly stuck and cannot make progress. Going silent does NOT end \
            the task — you must call this. Do not mark complete on unproven or \
            indirect evidence, and do not redefine success down to what already \
            exists."
            .to_string(),
        input_schema: serde_json::json!({
            "type": "object",
            "properties": {
                "status": {
                    "type": "string",
                    "enum": ["complete", "blocked"],
                    "description": "complete = proven done; blocked = truly stuck."
                },
                "summary": {
                    "type": "string",
                    "description": "Internal audit note only (not shown as a chat row on success). Keep ≤12 words for complete; longer only when blocked. Do not restate the user question or list files you read."
                },
                "next_step": {
                    "type": "string",
                    "description": "Optional concrete follow-up for the user. Omit this field when no genuine next step remains; never copy or paraphrase the conversation merely to fill it."
                },
                "override_incomplete_todos": {
                    "type": "boolean",
                    "description": "Set true only when deliberately closing despite incomplete plan todos (override must be allowed). A second bare complete without this flag is still refused."
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

/// Delivery: mark one plan step complete with a verification evidence ref.
pub(crate) const COMPLETE_STEP_TOOL: &str = "complete_step";

pub(crate) fn complete_step_tool_definition() -> ToolDefinition {
    ToolDefinition {
        name: COMPLETE_STEP_TOOL.to_string(),
        description: "Mark one plan step completed with a verification evidence \
            reference (tool_call_id of a successful verification run_command). \
            Required under Delivery work profile for multi-step plans before \
            update_goal(complete)."
            .to_string(),
        input_schema: serde_json::json!({
            "type": "object",
            "properties": {
                "step_id": {
                    "type": "string",
                    "description": "Plan step id or exact step text."
                },
                "summary": {
                    "type": "string",
                    "description": "What was done for this step."
                },
                "evidence_ref": {
                    "type": "string",
                    "description": "tool_call_id of a successful verification command."
                }
            },
            "required": ["step_id", "summary", "evidence_ref"]
        }),
    }
}

/// The tool the model calls to run a focused sub-agent on a self-contained
/// subtask, getting back only its final result. Emitting several calls in one
/// turn runs the sub-agents CONCURRENTLY.
pub(crate) fn spawn_agent_tool_definition() -> ToolDefinition {
    ToolDefinition {
        name: SPAWN_AGENT_TOOL.to_string(),
        description: "Delegate a self-contained subtask to a focused sub-agent so it \
            does not consume this conversation's context. The child shares your model \
            and workspace but starts a FRESH conversation — put everything it needs \
            in `task`. This tool runs the child IN THE BACKGROUND BY DEFAULT: the \
            call returns immediately with the child's id, you continue working, and \
            the runtime tells you when it settles (its truthful result, partial work \
            included). Start independent delegations together in one assistant \
            message; they run concurrently. \
            role='worker' performs scoped writes inside `files` (its exclusive \
            ownership — your own edits there are refused while it runs) and can run \
            targeted verification; assign disjoint `files` to parallel workers. \
            role='explorer' is read-only investigation/Q&A. \
            Keep the work yourself when it is small, tightly coupled, or needs your \
            in-flight context. Do NOT spawn the whole task as one blob. \
            agent='<name>' runs a reusable named persona (project \
            `.leveler/agents/<name>.md`, user-level, or built-in); its instructions \
            are prepended to `task` and supply the role unless you override it."
            .to_string(),
        input_schema: serde_json::json!({
            "type": "object",
            "properties": {
                "task": { "type": "string", "description": "The complete, self-contained instruction for the sub-agent." },
                "role": {
                    "type": "string",
                    "enum": ["default", "explorer", "worker"],
                    "description": "explorer = read-only investigation; worker = writes code within `files`; default = full tools."
                },
                "files": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description": "For role='worker': the paths this agent exclusively owns and may edit. Each entry is a relative file path or a directory (a directory grants its whole subtree, e.g. 'src/output/'). Edits outside them are rejected."
                },
                "agent": {
                    "type": "string",
                    "description": "Name of a reusable agent persona to run (project `.leveler/agents/<name>.md`, user-level, or built-in). Its instructions are prepended to `task`."
                },
                "run_in_background": {
                    "type": "boolean",
                    "description": "Defaults to true — the call returns immediately with the child's id and you continue useful work; the runtime tells you when it settles. Set false only when your next action depends on this child's result."
                }
            },
            "required": ["task"]
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

/// The tool the PARENT calls to judge an adopted finding.
pub(crate) const RESOLVE_FINDING_TOOL: &str = "resolve_finding";

pub(crate) fn report_finding_tool_definition() -> ToolDefinition {
    ToolDefinition {
        name: REPORT_FINDING_TOOL.to_string(),
        description: "Report ONE concrete finding the moment you confirm it \
            (do not batch them into your final prose). Each finding is recorded \
            durably and handed to the agent that spawned you even if your run is \
            cut short. Use one call per finding; keep `summary` specific and \
            evidence-backed, naming the file/symbol where it applies."
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
                "symbol": { "type": "string", "description": "The symbol it concerns, if any." },
                "blocking": {
                    "type": "boolean",
                    "description": "Reviewer only: this defect must be resolved before the change can be considered verified."
                }
            },
            "required": ["kind", "summary"]
        }),
    }
}

pub(crate) fn resolve_finding_tool_definition() -> ToolDefinition {
    ToolDefinition {
        name: RESOLVE_FINDING_TOOL.to_string(),
        description: "Judge one finding a sub-agent reported (ids look like \
            `f-3`). accepted = relevant, act on it; rejected = will not act, \
            requires `reason`; addressed = you made the fix for an accepted \
            finding (it is auto-verified once fresh verification passes). A \
            blocking finding must reach rejected or verified before the goal \
            can complete."
            .to_string(),
        input_schema: serde_json::json!({
            "type": "object",
            "properties": {
                "id": { "type": "string", "description": "The finding id, e.g. f-3." },
                "resolution": {
                    "type": "string",
                    "enum": ["accepted", "rejected", "addressed"],
                    "description": "Your judgment."
                },
                "reason": {
                    "type": "string",
                    "description": "Required for rejected: why this finding will not be acted on."
                }
            },
            "required": ["id", "resolution"]
        }),
    }
}

/// Turn-scoped elevations from an approved `request_permissions` call.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct TurnPermissionGrants {
    pub network: bool,
    /// Drop OS write_root confinement for run_command/shell_command this turn.
    pub unrestricted_fs: bool,
}

impl TurnPermissionGrants {
    pub fn is_empty(self) -> bool {
        !self.network && !self.unrestricted_fs
    }

    pub fn merge(self, other: Self) -> Self {
        Self {
            network: self.network || other.network,
            unrestricted_fs: self.unrestricted_fs || other.unrestricted_fs,
        }
    }
}

/// Parse model arguments for `request_permissions` (current fields plus legacy aliases).
///
/// - `network` (bool): request network for the rest of the turn
/// - `filesystem`: `"unrestricted"` | `"workspace"` (default) — unrestricted
///   clears write confinement (approximate full-access for commands)
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

    let full_access = args
        .get("full_access")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let network_explicit = args.get("network").and_then(|v| v.as_bool());
    let filesystem = args
        .get("filesystem")
        .and_then(|v| v.as_str())
        .unwrap_or("");

    let mut grants = TurnPermissionGrants::default();
    if full_access {
        grants.network = true;
        grants.unrestricted_fs = true;
    } else {
        let any_new_field = network_explicit.is_some() || !filesystem.is_empty();
        if any_new_field {
            grants.network = network_explicit.unwrap_or(false);
            grants.unrestricted_fs = matches!(
                filesystem,
                "unrestricted" | "full" | "full_access" | "danger-full-access"
            );
        } else {
            // Legacy: only action/reason → network elevation.
            grants.network = true;
        }
    }
    (action, reason, grants)
}

pub(crate) fn permission_request_description(
    action: &str,
    reason: &str,
    grants: TurnPermissionGrants,
) -> String {
    let mut parts = Vec::new();
    if grants.network {
        parts.push("网络");
    }
    if grants.unrestricted_fs {
        parts.push("文件系统(本轮写不受工作区沙箱限制,近似 full-access)");
    }
    let what = if parts.is_empty() {
        "权限".to_string()
    } else {
        parts.join("+")
    };
    if reason.is_empty() {
        format!("模型请求{what}:{action}")
    } else {
        format!("模型请求{what}:{action}(原因:{reason})")
    }
}

pub(crate) fn permission_grant_message(granted: bool, grants: TurnPermissionGrants) -> String {
    if !granted {
        return "用户未批准该权限,请改用不需要它的方式,或不要重复请求。".to_string();
    }
    let mut parts = Vec::new();
    if grants.network {
        parts.push("网络访问");
    }
    if grants.unrestricted_fs {
        parts.push("本轮无限制写(命令不再套工作区 write_root)");
    }
    if parts.is_empty() {
        "已获授权。".to_string()
    } else {
        format!("已获授权:本轮允许{}。", parts.join("、"))
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
    if grants.unrestricted_fs {
        ctx.policy.grant_unrestricted_fs();
    }
    ctx
}

/// The tool the model calls to ask for elevated permission (network / filesystem).
pub(crate) fn request_permissions_tool_definition() -> ToolDefinition {
    ToolDefinition {
        name: REQUEST_PERMISSIONS_TOOL.to_string(),
        description: "Ask the user to grant elevated permission for this turn. \
            Use BEFORE an action that needs network and/or writes outside the \
            workspace sandbox. Fields: `network` (bool), `filesystem` \
            (\"workspace\"|\"unrestricted\"), or `full_access` (bool) for both. \
            Legacy calls with only `action` still mean network-only. On approval, \
            grants last for the rest of this turn."
            .to_string(),
        input_schema: serde_json::json!({
            "type": "object",
            "properties": {
                "action": { "type": "string", "description": "What you want to do that needs permission." },
                "reason": { "type": "string", "description": "Why it is necessary." },
                "network": { "type": "boolean", "description": "Request network access for this turn." },
                "filesystem": {
                    "type": "string",
                    "enum": ["workspace", "unrestricted"],
                    "description": "workspace = keep write sandbox (default); unrestricted = no write_root for commands this turn."
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
            files_desc.contains("directory"),
            "files scope must advertise directory grants: {files_desc}"
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
        assert!(d.contains("background by default"), "{}", def.description);
        assert!(d.contains("returns immediately"), "{}", def.description);
        assert!(d.contains("settle"), "{}", def.description);
        assert!(
            d.contains("scoped") && (d.contains("write") || d.contains("edit")),
            "Worker scoped write must stay model-visible: {}",
            def.description
        );
        assert!(
            d.contains("keep the work yourself when"),
            "KEEP guidance must stay: {}",
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
        assert!(param.contains("next action depends"), "{param}");
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
                unrestricted_fs: true,
            },
        );
        assert!(!elevated.policy.network_denied());
        assert!(elevated.policy.unrestricted_fs());
        std::fs::remove_dir_all(&dir).ok();
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
            properties.contains_key("override_incomplete_todos"),
            "explicit todo override flag must be advertised"
        );
        let required = tool.input_schema["required"].as_array().unwrap();
        assert!(
            !required.iter().any(|field| field == "next_step"),
            "next_step must be omitted when there is no genuine follow-up"
        );
    }

    #[test]
    fn request_user_input_is_primary_with_ask_user_alias() {
        assert!(is_user_input_tool(REQUEST_USER_INPUT_TOOL));
        assert!(is_user_input_tool(ASK_USER_TOOL));
        assert!(!is_user_input_tool("request_permissions"));

        let primary = request_user_input_tool_definition();
        assert_eq!(primary.name, "request_user_input");
        assert!(primary.description.contains("ask_user"));
        assert!(primary.input_schema["properties"].get("question").is_some());
        assert!(
            primary.description.contains("mutually exclusive")
                || primary.description.contains("2–4")
                || primary.description.contains("2-4"),
            "tool description must require structured choice options: {}",
            primary.description
        );
        assert!(
            primary.description.contains("fake pause")
                || primary.description.contains("waiting for confirmation"),
            "tool description must ban prose-only waiting: {}",
            primary.description
        );

        let legacy = ask_user_tool_definition();
        assert_eq!(legacy.name, "ask_user");
        assert!(legacy.description.contains("request_user_input"));
        assert_eq!(
            primary.input_schema["required"],
            legacy.input_schema["required"]
        );
    }
}
