//! Agent authoring from the conversation: `list_agents`, `save_agent`,
//! `delete_agent`.
//!
//! The model proposes; it is not the authority. Every proposal is validated
//! before anyone is asked (a contradiction never reaches the user as a
//! question), and every write needs a human's yes in every permission profile,
//! full access included — like a durable memory, an agent definition changes
//! what future sessions will do. The approval prompt shows what is being
//! granted: scope, capability, write bounds, tools, model, skills and the
//! instructions. The write itself goes through [`AgentStore`].
//!
//! These tools belong to the top-level agent only: a delegated child never
//! holds them (see `ChildProfile::apply_to_registry`).

use std::path::{Path, PathBuf};

use async_trait::async_trait;
use schemars::JsonSchema;
use serde::Deserialize;
use tokio_util::sync::CancellationToken;

use leveler_execution::RiskLevel;
use leveler_tools::tools::{parse_input, schema_of};
use leveler_tools::{Tool, ToolContext, ToolError, ToolOutput};

use super::{
    AGENT_SCHEMA_VERSION, AgentCapability, AgentDefinition, AgentManifest, AgentRegistry,
    AgentRoots, AgentScope, AgentState, AgentStore, BudgetSpec, WorkspaceSpec,
    builtin::is_builtin_name, render_manifest, resolve_manifest,
};

pub const LIST_AGENTS_TOOL: &str = "list_agents";
pub const SAVE_AGENT_TOOL: &str = "save_agent";
pub const DELETE_AGENT_TOOL: &str = "delete_agent";

/// Tools a delegated child never holds.
pub const AUTHORING_TOOLS: &[&str] = &[LIST_AGENTS_TOOL, SAVE_AGENT_TOOL, DELETE_AGENT_TOOL];

/// How much of the instructions the approval prompt quotes.
const PREVIEW_INSTRUCTIONS_CHARS: usize = 1200;

/// The roots the authoring tools act on: this workspace, and the user agents
/// directory of the home in the tool's own environment.
fn roots_for(context: &ToolContext) -> AgentRoots {
    AgentRoots {
        project_root: Some(context.execution.workspace.root().to_path_buf()),
        user_agents_dir: leveler_core::leveler_home_dir_from(|k| {
            context.execution.environment.var_os(k)
        })
        .map(|root| leveler_core::LevelerHome::from_root(root).agents_dir()),
    }
}

/// The resolved skill registry in the tool's own environment, so an agent
/// definition is validated against the skills the runtime will actually load.
fn skill_registry_for(context: &ToolContext) -> leveler_skills::SkillRegistry {
    let root = context.execution.workspace.root();
    leveler_skills::SkillRegistry::load(&leveler_skills::SkillRoots::for_project_in(root, &|k| {
        context.execution.environment.var_os(k)
    }))
}

#[derive(Debug, Clone, Copy, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum SaveAction {
    Create,
    Update,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct SaveArgs {
    /// `project`: this repository's `.leveler/agents/`, shared through git.
    /// `user`: the user's own agents, available in every project. Use what the
    /// user asked for; if they did not say, ask them.
    scope: AgentScope,
    /// `create` a new agent or `update` an existing one in `scope`.
    action: SaveAction,
    /// Lowercase letters, digits and `-`, starting with a letter.
    name: String,
    /// One line the main agent reads when choosing agents.
    description: String,
    /// `read_only` (no writes), `writer` (claims its own write scope) or
    /// `scoped_writer` (given exclusive files at spawn).
    capability: AgentCapability,
    /// `provider/model`; omit to run on the parent's model.
    #[serde(default)]
    model: Option<String>,
    /// minimal | low | medium | high | xhigh | max; omit for the default.
    #[serde(default)]
    reasoning_effort: Option<String>,
    /// Installed skill names bound into every spawn.
    #[serde(default)]
    skills: Option<Vec<String>>,
    /// Narrow the capability's toolset to these tool names; omit for all of them.
    #[serde(default)]
    tools: Option<Vec<String>>,
    /// Writer capabilities only: repository-relative directories or files the
    /// agent may ever write. Omit to leave writes bounded only by what it claims.
    #[serde(default)]
    write_roots: Option<Vec<String>>,
    #[serde(default)]
    max_rounds: Option<u32>,
    #[serde(default)]
    max_duration_secs: Option<u64>,
    /// Role-specific guidance only: how to do this job. No runtime rules, tool
    /// schemas, permission mechanics or credentials.
    instructions: String,
}

impl SaveArgs {
    fn manifest(&self) -> AgentManifest {
        let budget =
            (self.max_rounds.is_some() || self.max_duration_secs.is_some()).then_some(BudgetSpec {
                max_rounds: self.max_rounds,
                max_duration_secs: self.max_duration_secs,
            });
        AgentManifest {
            version: AGENT_SCHEMA_VERSION,
            name: self.name.trim().to_string(),
            description: self.description.trim().to_string(),
            capability: self.capability,
            model: self.model.clone().filter(|m| !m.trim().is_empty()),
            reasoning_effort: self
                .reasoning_effort
                .clone()
                .filter(|e| !e.trim().is_empty()),
            skills: self.skills.clone().unwrap_or_default(),
            tools: self.tools.clone(),
            workspace: self
                .write_roots
                .clone()
                .map(|write_roots| WorkspaceSpec { write_roots }),
            budget,
        }
    }
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct DeleteArgs {
    scope: AgentScope,
    name: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct ListArgs {
    /// Show one agent's full definition (its agent.yaml and instructions)
    /// instead of the list — use it to copy a built-in into a new agent.
    #[serde(default)]
    name: Option<String>,
}

/// Whether `tool` writes an agent definition and therefore always needs a
/// human's confirmation.
pub fn is_agent_definition_write(tool: &str) -> bool {
    tool == SAVE_AGENT_TOOL || tool == DELETE_AGENT_TOOL
}

/// Validate a `save_agent` / `delete_agent` call before anyone is asked, and
/// render what the approval prompt shows. `Err` is the refusal the model gets.
pub(crate) fn preflight(
    tool: &str,
    arguments: &serde_json::Value,
    context: &ToolContext,
) -> Result<String, String> {
    let roots = roots_for(context);
    let registry = AgentRegistry::load(&roots);
    let skills = skill_registry_for(context);
    match tool {
        SAVE_AGENT_TOOL => {
            let args: SaveArgs = serde_json::from_value(arguments.clone())
                .map_err(|e| format!("save_agent: {e}"))?;
            let (manifest, dir) = check_save(&args, &roots, &registry, &skills)?;
            Ok(save_preview(&args, &manifest, &dir))
        }
        DELETE_AGENT_TOOL => {
            let args: DeleteArgs = serde_json::from_value(arguments.clone())
                .map_err(|e| format!("delete_agent: {e}"))?;
            let dir = scope_dir(&roots, args.scope)?.join(args.name.trim());
            if is_builtin_name(args.name.trim()) && !dir.is_dir() {
                return Err(format!(
                    "\"{}\" is a built-in agent and cannot be deleted",
                    args.name.trim()
                ));
            }
            if !dir.is_dir() {
                return Err(format!(
                    "{} agent \"{}\" does not exist",
                    args.scope.as_str(),
                    args.name.trim()
                ));
            }
            Ok(format!(
                "Delete {} agent \"{}\"\nLocation: {}\nRunning children spawned from it keep \
                 their definition; future spawns will not find it here.",
                args.scope.as_str(),
                args.name.trim(),
                dir.display()
            ))
        }
        _ => Err(format!("{tool} is not an agent authoring tool")),
    }
}

fn scope_dir(roots: &AgentRoots, scope: AgentScope) -> Result<PathBuf, String> {
    match scope {
        AgentScope::Project => roots
            .project_root
            .as_deref()
            .map(super::project_agents_dir)
            .ok_or_else(|| "no project is open".to_string()),
        AgentScope::User => roots.user_agents_dir.clone().ok_or_else(|| {
            "no CodeLeveler home is known, so there is no user agents directory".to_string()
        }),
    }
}

fn check_save(
    args: &SaveArgs,
    roots: &AgentRoots,
    registry: &AgentRegistry,
    skills: &leveler_skills::SkillRegistry,
) -> Result<(AgentManifest, PathBuf), String> {
    let manifest = args.manifest();
    let name = manifest.name.as_str();
    if is_builtin_name(name) {
        let copy = if super::is_reserved_agent_name(name) {
            String::new()
        } else {
            " (list_agents with its name shows its definition to copy)".to_string()
        };
        return Err(format!(
            "\"{name}\" is a built-in agent and cannot be written; save a {} agent under a \
             different name instead{copy}",
            args.scope.as_str()
        ));
    }
    let dir = scope_dir(roots, args.scope)?.join(name);
    resolve_manifest(
        &manifest,
        &args.instructions,
        name,
        match args.scope {
            AgentScope::Project => super::AgentSource::Project,
            AgentScope::User => super::AgentSource::User,
        },
        Some(dir.clone()),
    )
    .map_err(|e| format!("the proposed agent \"{name}\" is invalid: {e}"))?;
    // The same resolved registry the runtime uses, so "installed" means the
    // same thing here as it does to `load_skill` and `$mention`.
    let unavailable: Vec<String> = manifest
        .skills
        .iter()
        .filter_map(|s| skills.resolve(s).err().map(|error| error.to_string()))
        .collect();
    if !unavailable.is_empty() {
        return Err(format!(
            "{}. Remove it from the proposal, or create the skill separately first.",
            unavailable.join("; ")
        ));
    }
    let exists = dir.is_dir();
    match (args.action, exists) {
        (SaveAction::Create, true) => Err(format!(
            "{} agent \"{name}\" already exists; use action=update to change it",
            args.scope.as_str()
        )),
        (SaveAction::Update, false) => Err(format!(
            "{} agent \"{name}\" does not exist{}",
            args.scope.as_str(),
            match registry.get(name) {
                Some(entry) => format!(
                    " (the active \"{name}\" is a {} agent)",
                    entry.source.as_str()
                ),
                None => String::new(),
            }
        )),
        _ => Ok((manifest, dir)),
    }
}

fn save_preview(args: &SaveArgs, manifest: &AgentManifest, dir: &Path) -> String {
    let verb = match args.action {
        SaveAction::Create => "Create",
        SaveAction::Update => "Update",
    };
    let writes = match (manifest.capability, &manifest.workspace) {
        (AgentCapability::ReadOnly, _) => "no writes".to_string(),
        (AgentCapability::Writer, None) => "writes files it claims, anywhere in the project".into(),
        (AgentCapability::ScopedWriter, None) => {
            "writes only the files it is given at spawn".into()
        }
        (_, Some(ws)) => format!("writes only under {}", ws.write_roots.join(", ")),
    };
    let tools = manifest
        .tools
        .as_ref()
        .map(|t| t.join(", "))
        .unwrap_or_else(|| format!("all {} tools", manifest.capability.as_str()));
    let instructions = args.instructions.trim_end();
    let quoted: String = instructions
        .chars()
        .take(PREVIEW_INSTRUCTIONS_CHARS)
        .collect();
    let more = if quoted.len() < instructions.len() {
        format!("\n… ({} bytes in total)", instructions.len())
    } else {
        String::new()
    };
    format!(
        "{verb} {scope} agent \"{name}\"\n\
         Location: {location}\n\
         Capability: {capability} — {writes}\n\
         Tools: {tools}\n\
         Model: {model}\n\
         Reasoning effort: {effort}\n\
         Skills: {skills}\n\
         Instructions:\n{quoted}{more}",
        scope = args.scope.as_str(),
        name = manifest.name,
        location = dir.display(),
        capability = manifest.capability.as_str(),
        model = manifest.model.as_deref().unwrap_or("the parent's model"),
        effort = manifest
            .reasoning_effort
            .as_deref()
            .unwrap_or("runtime default"),
        skills = if manifest.skills.is_empty() {
            "none".to_string()
        } else {
            manifest.skills.join(", ")
        },
    )
}

pub struct ListAgentsTool;

#[async_trait]
impl Tool for ListAgentsTool {
    fn name(&self) -> &'static str {
        LIST_AGENTS_TOOL
    }

    fn description(&self) -> &'static str {
        "List the agents available to spawn_agent — built-in, user and project — with \
         each one's source, capability and whether it is usable. Pass `name` to see one \
         agent's full agent.yaml and instructions (for example to copy a built-in into a \
         new agent)."
    }

    fn input_schema(&self) -> serde_json::Value {
        schema_of::<ListArgs>()
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
        let args: ListArgs = parse_input(self.name(), input)?;
        let registry = AgentRegistry::load(&roots_for(&context));
        let Some(name) = args
            .name
            .as_deref()
            .map(str::trim)
            .filter(|n| !n.is_empty())
        else {
            return Ok(ToolOutput::ok(render_list(&registry)));
        };
        let Some(entry) = registry.get(name) else {
            return Ok(ToolOutput::error(format!(
                "Agent \"{name}\" not found. Available agents: {}.",
                registry.spawnable_names().join(", ")
            )));
        };
        let mut out = format!("{} ({} agent", entry.name, entry.source.as_str());
        if let Some(location) = &entry.location {
            out.push_str(&format!(" at {}", location.display()));
        }
        out.push_str(")\n");
        match &entry.state {
            AgentState::Invalid(error) => out.push_str(&format!("invalid: {error}\n")),
            AgentState::Valid(def) if def.is_structural() => out.push_str(&format!(
                "capability: {}\n{}\nA structural built-in: its behaviour is the runtime's \
                 role contract and it has no instructions file.\n",
                def.capability.as_str(),
                def.description
            )),
            AgentState::Valid(def) => {
                out.push_str(&format!(
                    "fingerprint: {}\n\nagent.yaml:\n",
                    def.fingerprint
                ));
                out.push_str(&render_manifest(&def.to_manifest()).unwrap_or_default());
                out.push_str("\ninstructions.md:\n");
                out.push_str(&def.instructions);
            }
        }
        for shadow in &entry.shadowed {
            out.push_str(&format!(
                "\nshadows a {} definition{}",
                shadow.source.as_str(),
                shadow
                    .location
                    .as_ref()
                    .map(|l| format!(" at {}", l.display()))
                    .unwrap_or_default()
            ));
        }
        Ok(ToolOutput::ok(out))
    }
}

fn render_list(registry: &AgentRegistry) -> String {
    let mut out = String::new();
    for entry in registry.entries() {
        let line = match &entry.state {
            AgentState::Valid(def) if def.is_harness_only() => format!(
                "- {} — {} (builtin, harness only)\n",
                def.name, def.description
            ),
            AgentState::Valid(def) => format!(
                "- {} — {} ({}, {}{})\n",
                def.name,
                def.description,
                def.capability.as_str(),
                entry.source.as_str(),
                if def.is_structural() {
                    format!(", runs as {}", def.role().label())
                } else {
                    String::new()
                }
            ),
            AgentState::Invalid(error) => format!(
                "- {} — INVALID {} agent: {error}\n",
                entry.name,
                entry.source.as_str()
            ),
        };
        out.push_str(&line);
    }
    for problem in registry.problems() {
        out.push_str(&format!(
            "- (not loaded) {}: {}\n",
            problem.location.display(),
            problem.error
        ));
    }
    out
}

impl AgentDefinition {
    /// The `agent.yaml` this definition would be written as.
    pub fn to_manifest(&self) -> AgentManifest {
        let budget =
            (self.max_rounds.is_some() || self.max_duration_secs.is_some()).then_some(BudgetSpec {
                max_rounds: self.max_rounds,
                max_duration_secs: self.max_duration_secs,
            });
        AgentManifest {
            version: AGENT_SCHEMA_VERSION,
            name: self.name.clone(),
            description: self.description.clone(),
            capability: self.capability,
            model: self.model.as_ref().map(ToString::to_string),
            reasoning_effort: self.reasoning_effort.map(|e| e.as_wire().to_string()),
            skills: self.skills.clone(),
            tools: self.tools.clone(),
            workspace: (!self.write_roots.is_empty()).then(|| WorkspaceSpec {
                write_roots: self.write_roots.clone(),
            }),
            budget,
        }
    }
}

pub struct SaveAgentTool;

#[async_trait]
impl Tool for SaveAgentTool {
    fn name(&self) -> &'static str {
        SAVE_AGENT_TOOL
    }

    fn description(&self) -> &'static str {
        "Create or update an agent definition (agent.yaml + instructions.md) when the user \
         asks for one. The user always confirms the exact proposal before anything is \
         written. Choose `scope` from what the user said — this project, or all their \
         projects — and ask if they did not say. Match capability and tools to what the \
         agent must do: a reviewer or analyst that must not change code is read_only."
    }

    fn input_schema(&self) -> serde_json::Value {
        schema_of::<SaveArgs>()
    }

    fn risk(&self) -> RiskLevel {
        // Policy asks a human for this tool in every profile regardless.
        RiskLevel::WorkspaceWrite
    }

    async fn execute(
        &self,
        input: serde_json::Value,
        context: ToolContext,
        _cancellation: CancellationToken,
    ) -> Result<ToolOutput, ToolError> {
        let args: SaveArgs = parse_input(self.name(), input)?;
        let roots = roots_for(&context);
        let registry = AgentRegistry::load(&roots);
        let skills = skill_registry_for(&context);
        let manifest = match check_save(&args, &roots, &registry, &skills) {
            Ok((manifest, _)) => manifest,
            Err(error) => return Ok(ToolOutput::error(error)),
        };
        let store = AgentStore::new(roots);
        let saved = match args.action {
            SaveAction::Create => store.create(args.scope, &manifest, &args.instructions),
            SaveAction::Update => store.update(args.scope, &manifest, &args.instructions),
        };
        Ok(match saved {
            Ok(def) => ToolOutput::ok(format!(
                "Saved {} agent \"{}\" at {} ({}). New spawns use it: \
                 spawn_agent(agent=\"{}\", task=...).",
                args.scope.as_str(),
                def.name,
                def.location
                    .as_ref()
                    .map(|l| l.display().to_string())
                    .unwrap_or_default(),
                def.fingerprint,
                def.name
            )),
            Err(error) => ToolOutput::error(error.to_string()),
        })
    }
}

pub struct DeleteAgentTool;

#[async_trait]
impl Tool for DeleteAgentTool {
    fn name(&self) -> &'static str {
        DELETE_AGENT_TOOL
    }

    fn description(&self) -> &'static str {
        "Delete a project or user agent definition when the user asks. The user confirms \
         first. Running children keep their definition; built-in agents cannot be deleted."
    }

    fn input_schema(&self) -> serde_json::Value {
        schema_of::<DeleteArgs>()
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
        let args: DeleteArgs = parse_input(self.name(), input)?;
        Ok(
            match AgentStore::new(roots_for(&context)).delete(args.scope, args.name.trim()) {
                Ok(()) => ToolOutput::ok(format!(
                    "Deleted {} agent \"{}\".",
                    args.scope.as_str(),
                    args.name.trim()
                )),
                Err(error) => ToolOutput::error(error.to_string()),
            },
        )
    }
}
