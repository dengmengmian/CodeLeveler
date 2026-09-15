//! `agent.yaml` schema v1 and its structural validation.
//!
//! Every struct denies unknown fields: a typo in a permission, tool, budget or
//! model key must fail the definition, never be ignored into a different agent.
//! There is deliberately no field that can carry a credential.

use serde::{Deserialize, Serialize};

use leveler_model::{ModelRef, ReasoningEffort};

use crate::child_profile::CHILD_MAX_DURATION_SECS;

/// The only schema version this build reads. `version` is required: a file
/// without it is refused rather than guessed at.
pub const AGENT_SCHEMA_VERSION: u32 = 1;

/// Longest `description`, in characters. The description is what the main
/// agent sees in its catalog on every turn, so it has to stay one line.
pub const MAX_DESCRIPTION_CHARS: usize = 200;

/// Largest `instructions.md`. Instructions enter every spawn of the agent's
/// initial context; 64 KiB (~16k tokens) is room for a thorough brief and
/// refuses a pasted log or manual.
pub const MAX_INSTRUCTIONS_BYTES: usize = 64 * 1024;

/// Largest `agent.yaml`.
pub const MAX_MANIFEST_BYTES: usize = 16 * 1024;

/// Upper bound on a definition's `budget.max_rounds`.
pub const MAX_AGENT_ROUNDS: u32 = 1000;

/// The runtime capability class an agent runs under. A class is an existing
/// child contract, not a new role: a new agent never needs a new enum variant.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum AgentCapability {
    /// Holds no mutating tool at all (the Explorer contract).
    ReadOnly,
    /// Starts read-capable and claims a write scope after reading (the
    /// Default child contract).
    Writer,
    /// Must be given an exclusive `files` scope at spawn (the Worker
    /// contract).
    ScopedWriter,
}

impl AgentCapability {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::ReadOnly => "read_only",
            Self::Writer => "writer",
            Self::ScopedWriter => "scoped_writer",
        }
    }

    pub fn can_write(self) -> bool {
        !matches!(self, Self::ReadOnly)
    }
}

/// `agent.yaml`, exactly as written on disk.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentManifest {
    pub version: u32,
    pub name: String,
    pub description: String,
    pub capability: AgentCapability,
    /// `provider/model`. Omitted: the child runs on the parent's model.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// `minimal|low|medium|high|xhigh|max`. Omitted: the runtime's default
    /// for the capability class.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_effort: Option<String>,
    /// Skill names bound into every spawn of this agent.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub skills: Vec<String>,
    /// Narrow the class's toolset to these tools. Omitted: the class's full
    /// set. Every listed tool is required to exist.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tools: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace: Option<WorkspaceSpec>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub budget: Option<BudgetSpec>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkspaceSpec {
    /// The most this agent may ever write: repository-relative files or
    /// directories. A spawn's actual scope must lie inside them.
    pub write_roots: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BudgetSpec {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_rounds: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_duration_secs: Option<u64>,
}

/// The manifest's fields after structural validation, typed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CheckedManifest {
    pub description: String,
    pub capability: AgentCapability,
    pub model: Option<ModelRef>,
    pub reasoning_effort: Option<ReasoningEffort>,
    pub skills: Vec<String>,
    pub tools: Option<Vec<String>>,
    pub write_roots: Vec<String>,
    pub max_rounds: Option<u32>,
    pub max_duration_secs: Option<u64>,
}

/// Parse `agent.yaml` text. Errors name the offending key.
pub fn parse_manifest(text: &str) -> Result<AgentManifest, String> {
    if text.len() > MAX_MANIFEST_BYTES {
        return Err(format!(
            "agent.yaml is {} bytes; the limit is {MAX_MANIFEST_BYTES}",
            text.len()
        ));
    }
    serde_yaml::from_str::<AgentManifest>(text).map_err(|e| format!("agent.yaml: {e}"))
}

/// Serialize a manifest in schema field order, so a UI save produces a stable,
/// reviewable diff.
pub fn render_manifest(manifest: &AgentManifest) -> Result<String, String> {
    serde_yaml::to_string(manifest).map_err(|e| format!("cannot render agent.yaml: {e}"))
}

impl AgentManifest {
    /// Structural validation: everything that can be decided from the file
    /// alone. Whether the model is configured or the skills exist on this
    /// machine is availability, not validity — see `AgentAvailability`.
    pub(crate) fn check(&self, dir_name: &str) -> Result<CheckedManifest, String> {
        if self.version != AGENT_SCHEMA_VERSION {
            return Err(format!(
                "unsupported version {}; this build reads version {AGENT_SCHEMA_VERSION}",
                self.version
            ));
        }
        super::name::validate_agent_name(&self.name)?;
        if self.name != dir_name {
            return Err(format!(
                "name `{}` does not match its directory `{dir_name}`",
                self.name
            ));
        }
        let description = self.description.trim();
        if description.is_empty() {
            return Err("description must not be empty".into());
        }
        if description.contains('\n') {
            return Err("description must be a single line".into());
        }
        if description.chars().count() > MAX_DESCRIPTION_CHARS {
            return Err(format!(
                "description is longer than {MAX_DESCRIPTION_CHARS} characters; \
                 put detail in instructions.md"
            ));
        }
        let model = match self.model.as_deref().map(str::trim) {
            None => None,
            Some(raw) => Some(
                ModelRef::parse(raw)
                    .ok_or_else(|| format!("model `{raw}` is not a `provider/model` reference"))?,
            ),
        };
        let reasoning_effort = match self.reasoning_effort.as_deref() {
            None => None,
            Some(raw) => Some(ReasoningEffort::parse(raw).ok_or_else(|| {
                format!(
                    "reasoning_effort `{raw}` is not one of minimal, low, medium, high, xhigh, max"
                )
            })?),
        };
        let mut skills = Vec::new();
        for skill in &self.skills {
            if !is_safe_skill_name(skill) {
                return Err(format!("skill `{skill}` is not a valid skill name"));
            }
            if skills.contains(skill) {
                return Err(format!("skill `{skill}` is listed twice"));
            }
            skills.push(skill.clone());
        }
        let tools = match &self.tools {
            None => None,
            Some(list) => Some(check_tools(list, self.capability)?),
        };
        let write_roots = match &self.workspace {
            None => Vec::new(),
            Some(_) if !self.capability.can_write() => {
                return Err("a read_only agent cannot declare workspace.write_roots; \
                     use capability writer or scoped_writer"
                    .into());
            }
            Some(spec) => check_write_roots(&spec.write_roots)?,
        };
        let (max_rounds, max_duration_secs) = match &self.budget {
            None => (None, None),
            Some(b) => {
                if let Some(r) = b.max_rounds
                    && !(1..=MAX_AGENT_ROUNDS).contains(&r)
                {
                    return Err(format!(
                        "budget.max_rounds must be between 1 and {MAX_AGENT_ROUNDS}"
                    ));
                }
                if let Some(d) = b.max_duration_secs
                    && !(1..=CHILD_MAX_DURATION_SECS).contains(&d)
                {
                    return Err(format!(
                        "budget.max_duration_secs must be between 1 and \
                         {CHILD_MAX_DURATION_SECS} (the runtime's child wall-clock cap)"
                    ));
                }
                (b.max_rounds, b.max_duration_secs)
            }
        };
        Ok(CheckedManifest {
            description: description.to_string(),
            capability: self.capability,
            model,
            reasoning_effort,
            skills,
            tools,
            write_roots,
            max_rounds,
            max_duration_secs,
        })
    }
}

fn is_safe_skill_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_'))
}

fn check_tools(list: &[String], capability: AgentCapability) -> Result<Vec<String>, String> {
    if list.is_empty() {
        return Err(
            "tools must not be an empty list; omit `tools` to use the capability's full set".into(),
        );
    }
    let mut out: Vec<String> = Vec::new();
    for tool in list {
        if tool.starts_with("mcp__") {
            return Err(format!(
                "tool `{tool}`: MCP tools cannot be granted to an agent; their effect \
                 cannot be bounded to a write scope"
            ));
        }
        if !leveler_tools::MODEL_SURFACE_TOOLS.contains(&tool.as_str()) {
            return Err(format!("unknown tool `{tool}`"));
        }
        if !capability.can_write() && !leveler_tools::is_observe_class_tool(tool) {
            return Err(format!(
                "tool `{tool}` is not read-only; a read_only agent may list only \
                 read-only tools"
            ));
        }
        if !out.contains(tool) {
            out.push(tool.clone());
        }
    }
    Ok(out)
}

/// Repository-relative, `/`-separated, no traversal, no globs.
fn check_write_roots(roots: &[String]) -> Result<Vec<String>, String> {
    if roots.is_empty() {
        return Err(
            "workspace.write_roots must not be empty; omit `workspace` to leave the \
             write scope unbounded by the definition"
                .into(),
        );
    }
    let mut out: Vec<String> = Vec::new();
    for raw in roots {
        let root = raw.trim().trim_end_matches('/');
        let bad = |why: &str| Err(format!("workspace.write_roots entry `{raw}`: {why}"));
        if root.is_empty() || root == "." {
            return bad("the repository root is not a bounded scope");
        }
        if root.starts_with('/') || root.contains('\\') || root.contains(':') {
            return bad("must be a repository-relative path using `/`");
        }
        if root
            .chars()
            .any(|c| matches!(c, '*' | '?' | '[' | ']' | '{' | '}'))
        {
            return bad("globs are not supported; name a directory or file");
        }
        if root
            .split('/')
            .any(|seg| seg.is_empty() || seg == "." || seg == "..")
        {
            return bad("must not contain empty, `.` or `..` segments");
        }
        if !out.iter().any(|r| r == root) {
            out.push(root.to_string());
        }
    }
    Ok(out)
}
