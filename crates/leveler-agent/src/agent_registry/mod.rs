//! Declarative agents: `agent.yaml` + `instructions.md`, one directory each.
//!
//! ```text
//! <repo>/.leveler/agents/<name>/agent.yaml        project (highest precedence)
//! <leveler-home>/agents/<name>/agent.yaml         user
//! compiled into the binary                        built-in
//! ```
//!
//! The registry is harness state, above the engine lifecycle: it decides which
//! agent definitions exist and whether each is usable. It does not decide when
//! an agent is used — that is the model's call — and it grants nothing by
//! itself. A definition maps onto an existing child capability class
//! ([`AgentCapability`]); every bound it declares can only narrow that class.
//!
//! Reading is side-effect free and fails closed per entry: one broken
//! definition is recorded as invalid and cannot be spawned, and it still
//! shadows lower-precedence definitions of the same name, so a typo never
//! silently falls back to a different agent.

mod authoring;
mod builtin;
mod manifest;
mod name;
mod runtime;
mod store;
#[cfg(test)]
mod store_tests;
#[cfg(test)]
mod tests;

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use leveler_model::{ModelRef, ReasoningEffort};

use crate::child_profile::{AgentRole, CHILD_MAX_DURATION_SECS, ChildProfile};

pub(crate) use authoring::preflight as authoring_preflight;
pub use authoring::{
    AUTHORING_TOOLS, DELETE_AGENT_TOOL, DeleteAgentTool, LIST_AGENTS_TOOL, ListAgentsTool,
    SAVE_AGENT_TOOL, SaveAgentTool, is_agent_definition_write,
};
pub use manifest::{
    AGENT_SCHEMA_VERSION, AgentCapability, AgentManifest, BudgetSpec, MAX_AGENT_ROUNDS,
    MAX_DESCRIPTION_CHARS, MAX_INSTRUCTIONS_BYTES, MAX_MANIFEST_BYTES, WorkspaceSpec,
    parse_manifest, render_manifest,
};
pub use name::{
    MAX_AGENT_NAME_LEN, RESERVED_AGENT_NAMES, is_reserved_agent_name, validate_agent_name,
};
pub use runtime::{
    CATALOG_HEADER, MAX_CATALOG_BYTES, effort_unsupported, render_brief, within_write_roots,
};
pub use store::{AgentScope, AgentStore, AgentStoreError};

/// The manifest file inside an agent directory.
pub const MANIFEST_FILE: &str = "agent.yaml";
/// The instructions file inside an agent directory.
pub const INSTRUCTIONS_FILE: &str = "instructions.md";

/// Where a definition came from, highest precedence first.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentSource {
    Project,
    User,
    Builtin,
}

impl AgentSource {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Project => "project",
            Self::User => "user",
            Self::Builtin => "builtin",
        }
    }
}

/// What kind of built-in or declared agent this is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AgentKind {
    /// Read from `agent.yaml` (including the compiled built-in personas).
    Declared,
    /// One of the runtime's structural roles (`default`, `explorer`,
    /// `worker`, `reviewer`). Its contract is [`ChildProfile::resolve`].
    Structural(AgentRole),
}

/// A validated, resolved agent definition.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentDefinition {
    pub name: String,
    pub description: String,
    pub source: AgentSource,
    /// The directory it was read from. `None` for built-ins.
    pub location: Option<PathBuf>,
    pub capability: AgentCapability,
    /// `None`: the child runs on the parent's model.
    pub model: Option<ModelRef>,
    /// `None`: the runtime's default for the capability class.
    pub reasoning_effort: Option<ReasoningEffort>,
    pub skills: Vec<String>,
    /// `None`: the capability class's full toolset.
    pub tools: Option<Vec<String>>,
    /// Empty: no definition-level bound beyond the class.
    pub write_roots: Vec<String>,
    pub max_rounds: Option<u32>,
    pub max_duration_secs: Option<u64>,
    /// Empty for structural built-ins, whose prompt is part of the runtime.
    pub instructions: String,
    /// `sha256:<hex>` over every field that shapes the child's behaviour.
    /// A debugging and inspection aid, not a security signature.
    pub fingerprint: String,
    pub(crate) kind: AgentKind,
}

impl AgentDefinition {
    /// The Reviewer: launched by the harness, never requested by a model.
    pub fn is_harness_only(&self) -> bool {
        matches!(self.kind, AgentKind::Structural(AgentRole::Reviewer))
    }

    /// One of `default`, `explorer`, `worker`, `reviewer`.
    pub fn is_structural(&self) -> bool {
        matches!(self.kind, AgentKind::Structural(_))
    }

    /// The runtime role this definition runs as.
    pub(crate) fn role(&self) -> AgentRole {
        match self.kind {
            AgentKind::Structural(role) => role,
            AgentKind::Declared => match self.capability {
                AgentCapability::ReadOnly => AgentRole::Explorer,
                AgentCapability::Writer => AgentRole::Default,
                AgentCapability::ScopedWriter => AgentRole::Worker,
            },
        }
    }

    /// The child capability contract this definition runs under: the class's
    /// built-in profile, named after the agent, with the definition's own
    /// bounds applied.
    pub(crate) fn child_profile(&self) -> ChildProfile {
        let mut profile = ChildProfile::resolve(self.role());
        if let AgentKind::Declared = self.kind {
            profile.id = self.name.clone();
            profile.name = self.name.clone();
            if self.max_rounds.is_some() {
                profile.runtime_policy.max_rounds = self.max_rounds;
            }
            if let Some(secs) = self.max_duration_secs {
                profile.budget_policy.max_duration_secs = secs.min(CHILD_MAX_DURATION_SECS);
            }
        }
        profile
    }
}

/// A registry entry's structural state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AgentState {
    Valid(Box<AgentDefinition>),
    /// The definition exists but cannot be used; the reason is actionable.
    Invalid(String),
}

/// A lower-precedence definition hidden by the active one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShadowedAgent {
    pub source: AgentSource,
    pub location: Option<PathBuf>,
}

/// One resolvable agent name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentEntry {
    pub name: String,
    pub source: AgentSource,
    pub location: Option<PathBuf>,
    pub state: AgentState,
    pub shadowed: Vec<ShadowedAgent>,
}

impl AgentEntry {
    pub fn definition(&self) -> Option<&AgentDefinition> {
        match &self.state {
            AgentState::Valid(def) => Some(def),
            AgentState::Invalid(_) => None,
        }
    }
}

/// Something under an agents directory that is not a resolvable name at all
/// (an invalid directory name, a reserved built-in name, an escaping root).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentProblem {
    pub source: AgentSource,
    pub location: PathBuf,
    pub error: String,
}

/// Why a name did not resolve to a spawnable definition.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ResolveError {
    NotFound {
        name: String,
        available: Vec<String>,
    },
    Invalid {
        name: String,
        source: AgentSource,
        location: Option<PathBuf>,
        error: String,
    },
    HarnessOnly {
        name: String,
    },
}

impl std::fmt::Display for ResolveError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotFound { name, available } => {
                write!(f, "Agent \"{name}\" not found.")?;
                let close: Vec<&str> = available
                    .iter()
                    .filter(|a| similar(name, a))
                    .map(String::as_str)
                    .collect();
                if !close.is_empty() {
                    write!(f, " Matching agents: {}.", close.join(", "))?;
                }
                write!(f, " Available agents: {}.", available.join(", "))
            }
            Self::Invalid {
                name,
                source,
                location,
                error,
            } => {
                write!(f, "Agent \"{name}\" ({} agent", source.as_str())?;
                if let Some(path) = location {
                    write!(f, " at {}", path.display())?;
                }
                write!(f, ") is invalid: {error}")
            }
            Self::HarnessOnly { name } => write!(
                f,
                "Agent \"{name}\" is launched by the harness for independent review and \
                 cannot be spawned. Use a read_only agent such as \"explorer\" to investigate."
            ),
        }
    }
}

fn similar(requested: &str, candidate: &str) -> bool {
    let r = requested.to_ascii_lowercase();
    candidate.contains(&r) || r.contains(candidate)
}

/// The directories a registry reads.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AgentRoots {
    /// The repository root; project agents live in `.leveler/agents` under it.
    pub project_root: Option<PathBuf>,
    /// `<leveler-home>/agents`.
    pub user_agents_dir: Option<PathBuf>,
}

impl AgentRoots {
    /// The project at `root` plus the user agents of the installed home.
    pub fn for_project(root: &Path) -> Self {
        Self {
            project_root: Some(root.to_path_buf()),
            user_agents_dir: user_agents_dir(),
        }
    }
}

/// `<repo>/.leveler/agents`.
pub fn project_agents_dir(root: &Path) -> PathBuf {
    root.join(".leveler").join("agents")
}

/// `<leveler-home>/agents`, if a home is known.
pub fn user_agents_dir() -> Option<PathBuf> {
    leveler_core::leveler_home_dir_from(|k| leveler_core::environment().var_os(k))
        .map(|root| leveler_core::LevelerHome::from_root(root).agents_dir())
}

/// Every agent visible from one project, resolved by precedence.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AgentRegistry {
    /// Sorted by name.
    entries: Vec<AgentEntry>,
    problems: Vec<AgentProblem>,
}

impl AgentRegistry {
    /// Read the built-ins and both directories. Never fails as a whole.
    pub fn load(roots: &AgentRoots) -> Self {
        let mut candidates: Vec<(String, AgentSource, Option<PathBuf>, AgentState)> = Vec::new();
        let mut problems = Vec::new();
        if let Some(root) = &roots.project_root {
            read_agents_dir(
                &project_agents_dir(root),
                AgentSource::Project,
                Some(root),
                &mut candidates,
                &mut problems,
            );
        }
        if let Some(dir) = &roots.user_agents_dir {
            read_agents_dir(dir, AgentSource::User, None, &mut candidates, &mut problems);
        }
        for def in builtin::definitions() {
            candidates.push((
                def.name.clone(),
                AgentSource::Builtin,
                None,
                AgentState::Valid(Box::new(def)),
            ));
        }
        // Stable: name, then precedence.
        candidates.sort_by(|a, b| a.0.cmp(&b.0).then(a.1.cmp(&b.1)));
        let mut entries: Vec<AgentEntry> = Vec::new();
        for (name, source, location, state) in candidates {
            match entries.last_mut() {
                Some(active) if active.name == name => {
                    active.shadowed.push(ShadowedAgent { source, location })
                }
                _ => entries.push(AgentEntry {
                    name,
                    source,
                    location,
                    state,
                    shadowed: Vec::new(),
                }),
            }
        }
        Self { entries, problems }
    }

    pub fn entries(&self) -> &[AgentEntry] {
        &self.entries
    }

    pub fn problems(&self) -> &[AgentProblem] {
        &self.problems
    }

    pub fn get(&self, name: &str) -> Option<&AgentEntry> {
        self.entries.iter().find(|e| e.name == name)
    }

    /// Resolve a name to a spawnable definition. Never falls back: an unknown
    /// name, an invalid active definition and the harness-only Reviewer are
    /// all refused with the reason.
    pub fn resolve(&self, name: &str) -> Result<&AgentDefinition, ResolveError> {
        let requested = name.trim();
        let Some(entry) = self.get(requested) else {
            return Err(ResolveError::NotFound {
                name: requested.to_string(),
                available: self.spawnable_names(),
            });
        };
        match &entry.state {
            AgentState::Invalid(error) => Err(ResolveError::Invalid {
                name: entry.name.clone(),
                source: entry.source,
                location: entry.location.clone(),
                error: error.clone(),
            }),
            AgentState::Valid(def) if def.is_harness_only() => Err(ResolveError::HarnessOnly {
                name: def.name.clone(),
            }),
            AgentState::Valid(def) => Ok(def),
        }
    }

    /// Names a model may spawn, sorted.
    pub fn spawnable_names(&self) -> Vec<String> {
        self.entries
            .iter()
            .filter(|e| e.definition().is_some_and(|d| !d.is_harness_only()))
            .map(|e| e.name.clone())
            .collect()
    }
}

fn read_agents_dir(
    dir: &Path,
    source: AgentSource,
    containing_root: Option<&Path>,
    candidates: &mut Vec<(String, AgentSource, Option<PathBuf>, AgentState)>,
    problems: &mut Vec<AgentProblem>,
) {
    let Ok(listing) = std::fs::read_dir(dir) else {
        return;
    };
    // A project's agents directory must be inside the project: a symlinked
    // `.leveler/agents` would let a repository point the registry anywhere.
    let Ok(canonical_dir) = dir.canonicalize() else {
        return;
    };
    if let Some(root) = containing_root {
        let inside = root
            .canonicalize()
            .is_ok_and(|root| canonical_dir.starts_with(root));
        if !inside {
            problems.push(AgentProblem {
                source,
                location: dir.to_path_buf(),
                error: "the agents directory resolves outside the project; no project agent \
                        was loaded"
                    .into(),
            });
            return;
        }
    }
    let mut names: Vec<(String, PathBuf)> = Vec::new();
    for entry in listing.flatten() {
        let path = entry.path();
        let Ok(file_type) = entry.file_type() else {
            continue;
        };
        let Some(dir_name) = entry.file_name().to_str().map(str::to_string) else {
            problems.push(AgentProblem {
                source,
                location: path,
                error: "the directory name is not valid UTF-8".into(),
            });
            continue;
        };
        // Hidden entries are staging directories and editor files.
        if dir_name.starts_with('.') {
            continue;
        }
        if !file_type.is_dir() && !file_type.is_symlink() {
            if file_type.is_file()
                && let Some(stem) = legacy_persona_name(&path, &dir_name)
            {
                problems.push(AgentProblem {
                    source,
                    location: path,
                    error: format!(
                        "legacy single-file agent persona; this format is no longer loaded. \
                         Migrate it to `{stem}/{MANIFEST_FILE}` and `{stem}/{INSTRUCTIONS_FILE}` \
                         in this agents directory"
                    ),
                });
            }
            continue;
        }
        if let Err(error) = validate_agent_name(&dir_name) {
            problems.push(AgentProblem {
                source,
                location: path,
                error,
            });
            continue;
        }
        if is_reserved_agent_name(&dir_name) {
            problems.push(AgentProblem {
                source,
                location: path,
                error: format!(
                    "`{dir_name}` is a built-in agent with runtime structural meaning and \
                     cannot be overridden; choose another name"
                ),
            });
            continue;
        }
        names.push((dir_name, path));
    }
    names.sort();
    for (name, path) in names {
        let state = match read_definition(&path, &canonical_dir, &name, source) {
            Ok(def) => AgentState::Valid(Box::new(def)),
            Err(error) => AgentState::Invalid(error),
        };
        candidates.push((name, source, Some(path), state));
    }
}

/// The agent name of a file in the single-file persona format that earlier
/// builds read: `<name>.md` directly in an agents directory, starting with a
/// frontmatter fence. Anything else (a README, notes) is not reported.
fn legacy_persona_name(path: &Path, file_name: &str) -> Option<String> {
    use std::io::Read;
    let stem = file_name.strip_suffix(".md")?;
    let old_name = !stem.is_empty()
        && stem
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_'));
    if !old_name {
        return None;
    }
    let mut head = Vec::new();
    std::fs::File::open(path)
        .ok()?
        .take(5)
        .read_to_end(&mut head)
        .ok()?;
    (head.starts_with(b"---\n") || head.starts_with(b"---\r\n")).then(|| stem.to_string())
}

fn read_definition(
    dir: &Path,
    canonical_agents_dir: &Path,
    name: &str,
    source: AgentSource,
) -> Result<AgentDefinition, String> {
    let meta = std::fs::symlink_metadata(dir).map_err(|e| format!("cannot read: {e}"))?;
    if meta.file_type().is_symlink() {
        return Err("symlinked agent directories are not followed".into());
    }
    let canonical = dir
        .canonicalize()
        .map_err(|e| format!("cannot resolve: {e}"))?;
    if canonical.parent() != Some(canonical_agents_dir) {
        return Err("the agent directory resolves outside its agents directory".into());
    }
    let manifest_text = read_regular_file(&dir.join(MANIFEST_FILE), MAX_MANIFEST_BYTES)?;
    let instructions = read_regular_file(&dir.join(INSTRUCTIONS_FILE), MAX_INSTRUCTIONS_BYTES)?;
    let manifest = parse_manifest(&manifest_text)?;
    resolve_manifest(
        &manifest,
        &instructions,
        name,
        source,
        Some(dir.to_path_buf()),
    )
}

/// Validate a manifest + instructions pair into a definition. The single
/// validation path: files on disk, compiled built-ins and authoring proposals
/// all go through it.
pub fn resolve_manifest(
    manifest: &AgentManifest,
    instructions: &str,
    dir_name: &str,
    source: AgentSource,
    location: Option<PathBuf>,
) -> Result<AgentDefinition, String> {
    let checked = manifest.check(dir_name)?;
    check_instructions(instructions)?;
    let mut def = AgentDefinition {
        name: manifest.name.clone(),
        description: checked.description,
        source,
        location,
        capability: checked.capability,
        model: checked.model,
        reasoning_effort: checked.reasoning_effort,
        skills: checked.skills,
        tools: checked.tools,
        write_roots: checked.write_roots,
        max_rounds: checked.max_rounds,
        max_duration_secs: checked.max_duration_secs,
        instructions: instructions.to_string(),
        fingerprint: String::new(),
        kind: AgentKind::Declared,
    };
    def.fingerprint = fingerprint(&def);
    Ok(def)
}

fn check_instructions(text: &str) -> Result<(), String> {
    if text.len() > MAX_INSTRUCTIONS_BYTES {
        return Err(format!(
            "{INSTRUCTIONS_FILE} is {} bytes; the limit is {MAX_INSTRUCTIONS_BYTES}. Keep \
             role-specific guidance there and link larger references from a skill",
            text.len()
        ));
    }
    if text.contains('\0') {
        return Err(format!("{INSTRUCTIONS_FILE} contains binary content"));
    }
    if text.trim().is_empty() {
        return Err(format!("{INSTRUCTIONS_FILE} is empty"));
    }
    Ok(())
}

fn read_regular_file(path: &Path, limit: usize) -> Result<String, String> {
    let file_name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let meta = match std::fs::symlink_metadata(path) {
        Ok(meta) => meta,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Err(format!("{file_name} is missing"));
        }
        Err(e) => return Err(format!("cannot read {file_name}: {e}")),
    };
    if !meta.file_type().is_file() {
        return Err(format!(
            "{file_name} must be a regular file (symlinks are not followed)"
        ));
    }
    if meta.len() > limit as u64 {
        return Err(format!(
            "{file_name} is {} bytes; the limit is {limit}",
            meta.len()
        ));
    }
    let bytes = std::fs::read(path).map_err(|e| format!("cannot read {file_name}: {e}"))?;
    String::from_utf8(bytes).map_err(|_| format!("{file_name} is not valid UTF-8"))
}

/// `sha256:<hex>` over the behaviour-shaping fields, in a fixed order. The
/// source and location are excluded: the same directory copied to another
/// project is the same agent.
fn fingerprint(def: &AgentDefinition) -> String {
    let input = serde_json::json!({
        "name": def.name,
        "description": def.description,
        "capability": def.capability.as_str(),
        "model": def.model.as_ref().map(ToString::to_string),
        "reasoning_effort": def.reasoning_effort.map(ReasoningEffort::as_wire),
        "skills": def.skills,
        "tools": def.tools,
        "write_roots": def.write_roots,
        "max_rounds": def.max_rounds,
        "max_duration_secs": def.max_duration_secs,
        "instructions": def.instructions,
    });
    let digest = Sha256::digest(input.to_string().as_bytes());
    let hex = digest.iter().fold(String::with_capacity(64), |mut out, b| {
        use std::fmt::Write as _;
        let _ = write!(out, "{b:02x}");
        out
    });
    format!("sha256:{hex}")
}

/// Whether a valid definition can run on this machine right now.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AgentAvailability {
    Available,
    Unavailable(String),
}

/// What availability depends on outside the definition itself.
pub trait AgentEnvironment {
    /// `Some(reason)` when `model` cannot be used (not configured, no key…).
    fn model_unavailable(&self, model: &ModelRef) -> Option<String>;
    /// `Some(reason)` when `effort` cannot be honoured exactly on the model
    /// the agent would run on (`None`: the parent's model).
    fn effort_unsupported(
        &self,
        model: Option<&ModelRef>,
        effort: ReasoningEffort,
    ) -> Option<String>;
    fn skill_exists(&self, name: &str) -> bool;
}

impl AgentDefinition {
    /// Structurally valid is not the same as usable: a configured model and
    /// installed skills are properties of this machine.
    pub fn availability(&self, env: &dyn AgentEnvironment) -> AgentAvailability {
        if let Some(model) = &self.model
            && let Some(reason) = env.model_unavailable(model)
        {
            return AgentAvailability::Unavailable(format!("model {model}: {reason}"));
        }
        if let Some(effort) = self.reasoning_effort
            && let Some(reason) = env.effort_unsupported(self.model.as_ref(), effort)
        {
            return AgentAvailability::Unavailable(format!(
                "reasoning_effort {}: {reason}",
                effort.as_wire()
            ));
        }
        let missing: Vec<&str> = self
            .skills
            .iter()
            .filter(|s| !env.skill_exists(s))
            .map(String::as_str)
            .collect();
        if !missing.is_empty() {
            return AgentAvailability::Unavailable(format!(
                "skill not found: {}",
                missing.join(", ")
            ));
        }
        AgentAvailability::Available
    }
}
