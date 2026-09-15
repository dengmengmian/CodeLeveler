//! The agent registry as the application serves it: to the client protocol,
//! the CLI and `doctor`.
//!
//! Every surface reads and writes definitions here, and this module only
//! projects [`leveler_agent::agent_registry`] — resolution, validation and the
//! atomic write path all live there, once.

use std::collections::HashMap;

use leveler_agent::agent_registry::{
    AGENT_SCHEMA_VERSION, AgentAvailability, AgentCapability, AgentDefinition, AgentEntry,
    AgentEnvironment, AgentManifest, AgentProblem, AgentRegistry, AgentRoots, AgentScope,
    AgentSource, AgentState, AgentStore, BudgetSpec, WorkspaceSpec,
};
use leveler_client_protocol::{
    UiAgentCapability, UiAgentDetail, UiAgentDraft, UiAgentEntry, UiAgentProblem, UiAgentScope,
    UiAgentSource, UiAgentStatus, UiChildAgentIdentity, UiShadowedAgent,
};
use leveler_model::{ModelProfile, ModelRef, ModelRuntime, ReasoningEffort};

use crate::Application;

/// What availability is checked against: the configured models (with their
/// profiles, read up front) and the skills installed for this project.
struct AppEnvironment {
    profiles: HashMap<ModelRef, ModelProfile>,
    session_model: ModelRef,
    root: std::path::PathBuf,
}

impl AgentEnvironment for AppEnvironment {
    fn model_unavailable(&self, model: &ModelRef) -> Option<String> {
        (!self.profiles.contains_key(model)).then(|| "not configured".to_string())
    }

    fn effort_unsupported(
        &self,
        model: Option<&ModelRef>,
        effort: ReasoningEffort,
    ) -> Option<String> {
        let model = model.unwrap_or(&self.session_model);
        match self.profiles.get(model) {
            Some(profile) => leveler_agent::agent_registry::effort_unsupported(profile, effort),
            None => Some(format!("model {model} is not configured")),
        }
    }

    fn skill_exists(&self, name: &str) -> bool {
        leveler_skills::load(&self.root, name).is_some()
    }
}

impl Application {
    /// The roots this project's agents are read from and written to.
    pub fn agent_roots(&self) -> AgentRoots {
        AgentRoots::for_project(&self.layout.repo_root)
    }

    pub fn agent_registry(&self) -> AgentRegistry {
        AgentRegistry::load(&self.agent_roots())
    }

    async fn agent_environment(
        &self,
        registry: &AgentRegistry,
        session_model: &ModelRef,
    ) -> AppEnvironment {
        let mut wanted: Vec<ModelRef> = registry
            .entries()
            .iter()
            .filter_map(AgentEntry::definition)
            .filter_map(|d| d.model.clone())
            .collect();
        wanted.push(session_model.clone());
        let configured = self.model_refs();
        let mut profiles = HashMap::new();
        for model in wanted {
            if configured.contains(&model)
                && !profiles.contains_key(&model)
                && let Ok(profile) = self.registry.profile(&model).await
            {
                profiles.insert(model, profile);
            }
        }
        AppEnvironment {
            profiles,
            session_model: session_model.clone(),
            root: self.layout.repo_root.clone(),
        }
    }

    /// Every resolvable agent with its status here, plus the directory
    /// entries that are not agents at all.
    pub async fn list_agents(
        &self,
        session_model: &ModelRef,
    ) -> (Vec<UiAgentEntry>, Vec<UiAgentProblem>) {
        let registry = self.agent_registry();
        let env = self.agent_environment(&registry, session_model).await;
        let agents = registry
            .entries()
            .iter()
            .map(|e| ui_entry(e, &env))
            .collect();
        let problems = registry.problems().iter().map(ui_problem).collect();
        (agents, problems)
    }

    /// One agent with its instructions.
    pub async fn get_agent(
        &self,
        name: &str,
        session_model: &ModelRef,
    ) -> Result<UiAgentDetail, String> {
        let registry = self.agent_registry();
        let Some(entry) = registry.get(name.trim()) else {
            return Err(format!(
                "Agent \"{}\" not found. Available agents: {}.",
                name.trim(),
                registry.spawnable_names().join(", ")
            ));
        };
        let env = self.agent_environment(&registry, session_model).await;
        let instructions = entry
            .definition()
            .filter(|d| !d.is_structural())
            .map(|d| d.instructions.clone());
        Ok(UiAgentDetail {
            entry: ui_entry(entry, &env),
            instructions,
        })
    }

    /// Create or update a definition from a client draft.
    pub async fn save_agent(
        &self,
        scope: UiAgentScope,
        draft: &UiAgentDraft,
        create: bool,
        session_model: &ModelRef,
    ) -> Result<UiAgentEntry, String> {
        let store = AgentStore::new(self.agent_roots());
        let manifest = manifest_from(draft);
        let scope = agent_scope(scope);
        let saved = if create {
            store.create(scope, &manifest, &draft.instructions)
        } else {
            store.update(scope, &manifest, &draft.instructions)
        };
        saved.map_err(|e| e.to_string())?;
        let registry = self.agent_registry();
        let env = self.agent_environment(&registry, session_model).await;
        registry
            .get(&manifest.name)
            .map(|entry| ui_entry(entry, &env))
            .ok_or_else(|| {
                format!(
                    "agent \"{}\" was written but does not resolve",
                    manifest.name
                )
            })
    }

    pub fn delete_agent(&self, scope: UiAgentScope, name: &str) -> Result<(), String> {
        AgentStore::new(self.agent_roots())
            .delete(agent_scope(scope), name.trim())
            .map_err(|e| e.to_string())
    }
}

fn agent_scope(scope: UiAgentScope) -> AgentScope {
    match scope {
        UiAgentScope::Project => AgentScope::Project,
        UiAgentScope::User => AgentScope::User,
    }
}

fn ui_source(source: AgentSource) -> UiAgentSource {
    match source {
        AgentSource::Project => UiAgentSource::Project,
        AgentSource::User => UiAgentSource::User,
        AgentSource::Builtin => UiAgentSource::Builtin,
    }
}

fn ui_capability(capability: AgentCapability) -> UiAgentCapability {
    match capability {
        AgentCapability::ReadOnly => UiAgentCapability::ReadOnly,
        AgentCapability::Writer => UiAgentCapability::Writer,
        AgentCapability::ScopedWriter => UiAgentCapability::ScopedWriter,
    }
}

fn manifest_from(draft: &UiAgentDraft) -> AgentManifest {
    let budget =
        (draft.max_rounds.is_some() || draft.max_duration_secs.is_some()).then_some(BudgetSpec {
            max_rounds: draft.max_rounds,
            max_duration_secs: draft.max_duration_secs,
        });
    AgentManifest {
        version: AGENT_SCHEMA_VERSION,
        name: draft.name.trim().to_string(),
        description: draft.description.trim().to_string(),
        capability: match draft.capability {
            UiAgentCapability::ReadOnly => AgentCapability::ReadOnly,
            UiAgentCapability::Writer => AgentCapability::Writer,
            UiAgentCapability::ScopedWriter => AgentCapability::ScopedWriter,
        },
        model: draft.model.clone().filter(|m| !m.trim().is_empty()),
        reasoning_effort: draft
            .reasoning_effort
            .clone()
            .filter(|e| !e.trim().is_empty()),
        skills: draft.skills.clone(),
        tools: draft.tools.clone(),
        workspace: (!draft.write_roots.is_empty()).then(|| WorkspaceSpec {
            write_roots: draft.write_roots.clone(),
        }),
        budget,
    }
}

fn ui_entry(entry: &AgentEntry, env: &dyn AgentEnvironment) -> UiAgentEntry {
    let shadowed = entry
        .shadowed
        .iter()
        .map(|s| UiShadowedAgent {
            source: ui_source(s.source),
            location: s.location.as_ref().map(|l| l.display().to_string()),
        })
        .collect();
    let base = UiAgentEntry {
        name: entry.name.clone(),
        source: ui_source(entry.source),
        location: entry.location.as_ref().map(|l| l.display().to_string()),
        status: UiAgentStatus::Invalid,
        reason: None,
        description: None,
        capability: None,
        structural: false,
        harness_only: false,
        model: None,
        reasoning_effort: None,
        skills: Vec::new(),
        tools: None,
        write_roots: Vec::new(),
        max_rounds: None,
        max_duration_secs: None,
        fingerprint: None,
        shadowed,
    };
    match &entry.state {
        AgentState::Invalid(error) => UiAgentEntry {
            reason: Some(error.clone()),
            ..base
        },
        AgentState::Valid(def) => {
            let (status, reason) = match def.availability(env) {
                AgentAvailability::Available => (UiAgentStatus::Available, None),
                AgentAvailability::Unavailable(reason) => {
                    (UiAgentStatus::Unavailable, Some(reason))
                }
            };
            UiAgentEntry {
                status,
                reason,
                ..defined(base, def)
            }
        }
    }
}

fn defined(base: UiAgentEntry, def: &AgentDefinition) -> UiAgentEntry {
    UiAgentEntry {
        description: Some(def.description.clone()),
        capability: Some(ui_capability(def.capability)),
        structural: def.is_structural(),
        harness_only: def.is_harness_only(),
        model: def.model.as_ref().map(ToString::to_string),
        reasoning_effort: def.reasoning_effort.map(|e| e.as_wire().to_string()),
        skills: def.skills.clone(),
        tools: def.tools.clone(),
        write_roots: def.write_roots.clone(),
        max_rounds: def.max_rounds,
        max_duration_secs: def.max_duration_secs,
        fingerprint: Some(def.fingerprint.clone()),
        ..base
    }
}

fn ui_problem(problem: &AgentProblem) -> UiAgentProblem {
    UiAgentProblem {
        source: ui_source(problem.source),
        location: problem.location.display().to_string(),
        error: problem.error.clone(),
    }
}

/// The client projection of a child's spawn-time agent snapshot.
pub fn child_agent_identity(
    spec: &leveler_lifecycle::ChildSpawnSpec,
) -> Option<UiChildAgentIdentity> {
    spec.agent.as_ref().map(|agent| UiChildAgentIdentity {
        name: agent.name.clone(),
        source: agent.source.clone(),
        capability: agent.capability.clone(),
        fingerprint: agent.fingerprint.clone(),
        model: spec.model.clone(),
        reasoning_effort: agent.reasoning_effort.clone(),
        skills: agent.skills.clone(),
    })
}
