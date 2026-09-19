//! The resolved skill registry as the application serves it: to the CLI and to
//! clients.
//!
//! This module only projects [`leveler_skills`] — resolution, precedence,
//! status and loading all live there, once. A client reading a skill and the
//! runtime loading it therefore always see the same answer.

use leveler_client_protocol::{
    UiShadowedSkill, UiSkillDetail, UiSkillEntry, UiSkillProblem, UiSkillScope, UiSkillSource,
    UiSkillStatus,
};
use leveler_skills::{
    SkillEntry, SkillLookupError, SkillProblem, SkillRegistry, SkillRoots, SkillScope, SkillSource,
    SkillState,
};

use crate::Application;

impl Application {
    /// The resolved registry for this project.
    pub fn skill_registry(&self) -> SkillRegistry {
        SkillRegistry::load(&SkillRoots::for_project_in(
            &self.layout.repo_root,
            &|key| self.environment.var_os(key),
        ))
    }

    /// Every entry with its status here, plus the directory entries that are not
    /// skills at all.
    pub fn list_skills(&self) -> (Vec<UiSkillEntry>, Vec<UiSkillProblem>) {
        let registry = self.skill_registry();
        let entries = registry.entries().iter().map(ui_entry).collect();
        let problems = registry.problems().iter().map(ui_problem).collect();
        (entries, problems)
    }

    /// One skill with its body and bundled files. An invalid skill still
    /// resolves to a detail record that explains why.
    pub fn get_skill(&self, name: &str) -> Result<UiSkillDetail, String> {
        let registry = self.skill_registry();
        let requested = name.trim();
        let Some(entry) = registry.get(requested) else {
            return Err(format!(
                "Skill \"{requested}\" not found. Available skills: {}.",
                registry.available_names().join(", ")
            ));
        };
        let loaded = match registry.load_skill(requested) {
            Ok(detail) => Some(detail),
            Err(SkillLookupError::Invalid { .. }) => None,
            Err(error) => return Err(error.to_string()),
        };
        Ok(UiSkillDetail {
            entry: ui_entry(entry),
            body: loaded.as_ref().map(|d| d.body.clone()),
            scripts: loaded
                .as_ref()
                .map(|d| d.scripts.clone())
                .unwrap_or_default(),
            references: loaded
                .as_ref()
                .map(|d| d.references.clone())
                .unwrap_or_default(),
            other_files: loaded
                .as_ref()
                .map(|d| d.other_files.clone())
                .unwrap_or_default(),
        })
    }
}

fn ui_scope(scope: SkillScope) -> UiSkillScope {
    match scope {
        SkillScope::Project => UiSkillScope::Project,
        SkillScope::User => UiSkillScope::User,
        SkillScope::Builtin => UiSkillScope::Builtin,
    }
}

fn ui_source(source: SkillSource) -> UiSkillSource {
    match source {
        SkillSource::Native => UiSkillSource::Native,
        SkillSource::Codex => UiSkillSource::Codex,
        SkillSource::AgentSkills => UiSkillSource::AgentSkills,
        SkillSource::Claude => UiSkillSource::Claude,
        SkillSource::Builtin => UiSkillSource::Builtin,
    }
}

fn ui_entry(entry: &SkillEntry) -> UiSkillEntry {
    let (status, reason) = match &entry.state {
        SkillState::Available => (UiSkillStatus::Available, None),
        SkillState::Invalid(reason) => (UiSkillStatus::Invalid, Some(reason.clone())),
    };
    UiSkillEntry {
        name: entry.name.clone(),
        scope: ui_scope(entry.scope),
        source: ui_source(entry.source),
        location: entry.location.as_ref().map(|l| l.display().to_string()),
        status,
        reason,
        description: entry.description.clone(),
        shadowed: entry
            .shadowed
            .iter()
            .map(|s| UiShadowedSkill {
                scope: ui_scope(s.scope),
                source: ui_source(s.source),
                location: s.location.as_ref().map(|l| l.display().to_string()),
                error: s.error.clone(),
            })
            .collect(),
    }
}

fn ui_problem(problem: &SkillProblem) -> UiSkillProblem {
    UiSkillProblem {
        scope: ui_scope(problem.scope),
        source: ui_source(problem.source),
        location: problem.location.display().to_string(),
        error: problem.error.clone(),
    }
}
