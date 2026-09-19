//! Spawn admission for `spawn_agent`: built-in roles and declarative agents.
//!
//! A declarative agent's class, toolset, write roots, model, effort and skills
//! come from its definition, resolved once here. The call can only repeat the
//! agent's class, never change it; everything the definition names must be
//! available exactly, or the spawn is refused with the reason.

use leveler_lifecycle::ChildSpawnSpec;
use leveler_model::ModelRef;

use super::Executor;
use crate::agent_registry::{
    AgentDefinition, AgentRegistry, AgentRoots, effort_unsupported, render_brief,
    within_write_roots,
};
use crate::child_profile::{AgentRole, ChildProfile};

/// What an admitted spawn runs as.
pub(crate) struct SpawnAdmission {
    pub profile: ChildProfile,
    /// Everything but `files` and `background`, which the call supplies.
    pub spec: ChildSpawnSpec,
    /// A declarative agent's instructions and bound skills.
    pub brief: Option<String>,
}

impl Executor {
    /// The roots declarative agents are read from.
    pub(crate) fn agent_roots(&self) -> AgentRoots {
        self.agent_roots.clone().unwrap_or_else(|| {
            AgentRoots::for_project(self.tool_context.execution.workspace.root())
        })
    }

    pub(crate) fn load_agent_registry(&self) -> AgentRegistry {
        AgentRegistry::load(&self.agent_roots())
    }

    /// Admit one `spawn_agent` call. `registry` is loaded by the caller once per
    /// batch, only when some call names an agent.
    pub(crate) async fn admit_spawn_call(
        &self,
        agent: Option<&str>,
        profile: Option<&str>,
        role: Option<&str>,
        files: &[String],
        registry: Option<&AgentRegistry>,
    ) -> Result<SpawnAdmission, String> {
        let Some(name) = agent else {
            return ChildProfile::admit_spawn(profile, role, files).map(|profile| SpawnAdmission {
                profile,
                spec: ChildSpawnSpec::default(),
                brief: None,
            });
        };
        let registry = registry.expect("the batch loads the registry when an agent is named");
        let def = registry.resolve(name).map_err(|e| e.to_string())?;
        check_class_request(def, "profile", profile)?;
        check_class_request(def, "role", role)?;
        if def.is_structural() {
            return ChildProfile::admit(def.role(), files).map(|profile| SpawnAdmission {
                profile,
                spec: ChildSpawnSpec::default(),
                brief: None,
            });
        }
        self.admit_declared(def, files).await
    }

    async fn admit_declared(
        &self,
        def: &AgentDefinition,
        files: &[String],
    ) -> Result<SpawnAdmission, String> {
        let name = &def.name;
        let profile = ChildProfile::admit_profile(def.child_profile(), files)
            .map_err(|e| format!("Agent \"{name}\": {e}"))?;
        if !def.write_roots.is_empty() {
            let outside: Vec<&str> = files
                .iter()
                .filter(|f| !within_write_roots(f, &def.write_roots))
                .map(String::as_str)
                .collect();
            if !outside.is_empty() {
                return Err(format!(
                    "Agent \"{name}\" may write only under {}; {} is outside. Narrow `files` \
                     to paths inside those roots.",
                    def.write_roots.join(", "),
                    outside.join(", ")
                ));
            }
        }
        if let Some(tools) = &def.tools {
            let class_registry = profile.apply_to_registry(&self.registry);
            let missing: Vec<&str> = tools
                .iter()
                .filter(|t| class_registry.get(t).is_none())
                .map(String::as_str)
                .collect();
            if !missing.is_empty() {
                return Err(format!(
                    "Agent \"{name}\" requires {} which {} not available in this session; \
                     the agent cannot run here.",
                    missing.join(", "),
                    if missing.len() == 1 { "is" } else { "are" }
                ));
            }
        }
        let root = self.tool_context.execution.workspace.root();
        // One resolved registry for every bound skill, built from this
        // execution's environment: the same answer `load_skill`, `$mention` and
        // `Agent.skills` see.
        let environment = &self.tool_context.execution.environment;
        let registry = leveler_skills::SkillRegistry::load(
            &leveler_skills::SkillRoots::for_project_in(root, &|key| environment.var_os(key)),
        );
        let mut skills = Vec::new();
        let mut unavailable = Vec::new();
        for skill in &def.skills {
            match registry.load_skill(skill) {
                Ok(detail) => skills.push(detail),
                Err(error) => unavailable.push(error.to_string()),
            }
        }
        if !unavailable.is_empty() {
            return Err(format!(
                "Agent \"{name}\" binds {}; the agent cannot run here.",
                unavailable.join("; ")
            ));
        }
        if let Some(model) = &def.model
            && let Some(refusal) = self.pinned_model_refusal(model).await
        {
            return Err(format!("Agent \"{name}\": {refusal}."));
        }
        if let Some(effort) = def.reasoning_effort {
            let model: &ModelRef = def.model.as_ref().unwrap_or(&self.model);
            let reason = match self.runtime.profile(model).await {
                Ok(profile) => effort_unsupported(&profile, effort),
                Err(error) => Some(format!("model {model} is not available: {error}")),
            };
            if let Some(reason) = reason {
                return Err(format!(
                    "Agent \"{name}\" asks for reasoning_effort {}, but {reason}; the agent \
                     cannot run here.",
                    effort.as_wire()
                ));
            }
        }
        Ok(SpawnAdmission {
            profile,
            spec: ChildSpawnSpec {
                model: def.model.as_ref().map(ToString::to_string),
                tools: def.tools.clone().unwrap_or_default(),
                max_rounds: def.max_rounds.unwrap_or(0),
                agent: Some(Box::new(def.snapshot())),
                ..ChildSpawnSpec::default()
            },
            brief: Some(render_brief(def, &skills)),
        })
    }
}

/// A call may repeat an agent's class (`role`/`profile`), never change it.
fn check_class_request(
    def: &AgentDefinition,
    field: &str,
    raw: Option<&str>,
) -> Result<(), String> {
    let Some(raw) = raw.map(str::trim).filter(|s| !s.is_empty()) else {
        return Ok(());
    };
    let requested = match field {
        "profile" => ChildProfile::lookup(raw).map(|p| p.role),
        _ => AgentRole::from_label(raw),
    };
    if requested == Some(def.role()) {
        return Ok(());
    }
    Err(format!(
        "Agent \"{}\" runs as capability {} (role {}); {field}='{raw}' does not match. \
         Omit `{field}`: an agent's class comes from its definition.",
        def.name,
        def.capability.as_str(),
        def.role().label()
    ))
}
