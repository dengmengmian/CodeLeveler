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
use leveler_core::SessionId;
use leveler_definition_trust::DefinitionTrust;
use leveler_execution::PermissionProfile;
use leveler_skills::{
    SkillCommandError, SkillEntry, SkillLookupError, SkillProblem, SkillRegistry, SkillRoots,
    SkillScope, SkillSource, SkillState, leading_skill_token, parse_skill_mentions,
};
use leveler_tools::definition_trust::DefinitionTrustAuthorizationOutcome;

use crate::Application;

impl Application {
    /// Why a mid-turn append cannot carry this Skill request, or `None` when
    /// the text is ordinary.
    ///
    /// A steer is appended as plain text at the next model round; it never
    /// rebuilds the turn's skill injection. Delivering `/name` or `$name`
    /// there would silently drop the procedure the user asked for, so the
    /// runtime refuses it before any input row is journaled.
    pub fn mid_turn_skill_refusal(&self, content: &str) -> Option<String> {
        // Ordinary steer text must not pay for a registry read on the hot
        // path. Both skill surfaces need either a leading `/` token or a `$`.
        if leading_skill_token(content).is_none() && !content.contains('$') {
            return None;
        }
        let registry = self.skill_registry();
        if let Some(token) = leading_skill_token(content) {
            match registry.resolve_command(&token, leveler_skills::RESERVED_SLASH_NAMES) {
                Ok(command) => {
                    return Some(format!(
                        "`/{}` 是技能调用：技能指令只能随新消息加载，任务运行中无法追加。该消息未发送，请在任务结束后发送。",
                        command.advertised
                    ));
                }
                Err(SkillCommandError::NotFound { .. } | SkillCommandError::Reserved { .. }) => {}
                Err(error) => {
                    return Some(format!(
                        "{error}；任务运行中不能追加技能调用，该消息未发送。"
                    ));
                }
            }
        }
        let name = parse_skill_mentions(content).into_iter().find(|name| {
            registry.get(name).is_some() || registry.untrusted_skill(name).is_some()
        })?;
        Some(format!(
            "`${name}` 是技能提及：技能指令只能随新消息加载，任务运行中无法追加。该消息未发送，请在任务结束后发送。"
        ))
    }
}

impl Application {
    /// Ask the human to admit a Skill the user explicitly invoked, before its
    /// turn starts.
    ///
    /// `load_skill` has the same gate for model discovery. This is the
    /// explicit `/name` lane: the message is the user's own instruction, but
    /// the package body is still untrusted input until a human admits these
    /// exact bytes. FullAccess never prompts; its session baseline decides.
    /// `Err` is the reason the submission must not start, so the client keeps
    /// the draft and no model turn is opened.
    pub(crate) async fn authorize_explicit_skill(
        &self,
        session_id: &SessionId,
        content: &str,
        mode: PermissionProfile,
        authorizer: &dyn leveler_tools::DefinitionTrustAuthorizer,
    ) -> Result<(), String> {
        let Some(token) = leading_skill_token(content) else {
            return Ok(());
        };
        let Some(project) = self.layout.primary_workspace() else {
            return Ok(());
        };
        let roots = SkillRoots::for_project_in(project, &|key| self.environment.var_os(key));
        // The same store + session overlay the turn's executor will read.
        let trust = self
            .definition_trust_for(Some(project))
            .with_session(self.definition_trust_for_session(session_id.as_str(), mode));
        let registry = SkillRegistry::load(
            &roots,
            &trust,
            &leveler_skills::PreferencesState::load(
                &leveler_core::LevelerHome::resolve(&self.environment),
                Some(project),
            ),
        );
        // Only an untrusted definition needs a human. Disabled, invalid,
        // model-only and shadowed names keep their own refusal paths; a
        // usable token, an unknown name or a reservation is the turn's
        // business, not an authorization question.
        let name = match registry.resolve_command(&token, leveler_skills::RESERVED_SLASH_NAMES) {
            Ok(_) => return Ok(()),
            Err(SkillCommandError::Untrusted { name, .. }) => name,
            Err(_) => return Ok(()),
        };
        let Some(package) = registry.untrusted_skill(&name) else {
            return Ok(());
        };
        if matches!(package.decision, DefinitionTrust::StoreUnavailable { .. }) {
            return Err(format!(
                "技能 /{name} 存在，但定义信任库不可用（{}），无法请求授权；本轮未发送。\
                 请修复信任库（见 leveler doctor）后使用 /trust 查看。",
                package.state()
            ));
        }
        let Some(identity) = package.identity.clone() else {
            return Ok(());
        };
        let (current_content, current_digest, content_truncated) =
            leveler_tools::definition_trust::skill_package_preview(&package.location).map_err(
                |error| format!("技能 /{name} 的当前内容无法读取以供审查（{error}）；本轮未发送。"),
            )?;
        let approved_content = package
            .decision
            .approved_digest()
            .and_then(|digest| {
                leveler_definition_trust::read_snapshot(
                    leveler_core::LevelerHome::resolve(&self.environment).root(),
                    digest,
                )
            })
            .map(|bytes| {
                leveler_tools::definition_trust::bounded_preview(&String::from_utf8_lossy(&bytes))
            });
        let request = leveler_tools::definition_trust::DefinitionTrustAuthorizationRequest {
            call_id: format!("explicit-skill:{name}"),
            identity,
            kind: leveler_definition_trust::DefinitionKind::Skill,
            scope: match package.scope {
                SkillScope::Project => leveler_definition_trust::DefinitionScope::Project,
                SkillScope::User => leveler_definition_trust::DefinitionScope::User,
                SkillScope::Builtin => return Ok(()),
            },
            source: package.source.label().to_string(),
            location: package.location.clone(),
            state: package.decision.clone(),
            current_digest,
            current_content,
            approved_content,
            content_truncated,
        };
        match authorizer.authorize(request).await {
            DefinitionTrustAuthorizationOutcome::AllowedOnce
            | DefinitionTrustAuthorizationOutcome::Trusted => Ok(()),
            DefinitionTrustAuthorizationOutcome::Denied => {
                Err(format!("已拒绝加载技能 /{name}；本轮未发送，草稿已保留。"))
            }
            DefinitionTrustAuthorizationOutcome::ContentChanged => Err(format!(
                "技能 /{name} 在审批期间发生变化，授权未覆盖当前内容；本轮未发送，请重试或使用 /trust 查看。"
            )),
            // FullAccess admits through its session baseline and never
            // prompts. A package outside that baseline keeps its refusal in
            // the turn's delivery audit instead of becoming an extra dialog
            // in Full.
            DefinitionTrustAuthorizationOutcome::Unavailable
                if mode == PermissionProfile::FullAccess =>
            {
                Ok(())
            }
            DefinitionTrustAuthorizationOutcome::Unavailable => Err(format!(
                "技能 /{name} 未信任，且当前没有可用的授权交互（客户端已断开或会话已取消）；本轮未发送。"
            )),
            DefinitionTrustAuthorizationOutcome::Failed(reason) => Err(format!(
                "技能 /{name} 的授权未能记录（{reason}）；本轮未发送。"
            )),
        }
    }

    /// The resolved registry for this project.
    pub fn skill_registry(&self) -> SkillRegistry {
        let project = self.layout.primary_workspace();
        let roots = project
            .map(|root| SkillRoots::for_project_in(root, &|key| self.environment.var_os(key)))
            .unwrap_or_else(SkillRoots::empty);
        SkillRegistry::load(
            &roots,
            &self.definition_trust_for(project),
            &leveler_skills::PreferencesState::load(
                &leveler_core::LevelerHome::resolve(&self.environment),
                project,
            ),
        )
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
        let loaded = match registry.load_skill_for_inspection(requested) {
            Ok(detail) => Some(detail),
            Err(SkillLookupError::Invalid { .. } | SkillLookupError::Disabled { .. }) => None,
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

/// The live client projection of a turn's skill delivery. Only explicitly
/// selected entries and host blocks are carried; an index-only turn sends
/// nothing because there is nothing the person asked for.
pub(crate) fn ui_delivery(
    audit: &leveler_skills::SkillDeliveryAudit,
) -> leveler_client_protocol::UiSkillDelivery {
    use leveler_client_protocol::{UiSkillDelivery, UiSkillDeliveryEntry, UiSkillDeliveryFailure};
    fn failure(value: leveler_skills::SkillDeliveryFailure) -> UiSkillDeliveryFailure {
        use leveler_skills::SkillDeliveryFailure as F;
        match value {
            F::NotFound => UiSkillDeliveryFailure::NotFound,
            F::Invalid => UiSkillDeliveryFailure::Invalid,
            F::Disabled => UiSkillDeliveryFailure::Disabled,
            F::Untrusted => UiSkillDeliveryFailure::Untrusted,
            F::UserInvocationDisabled => UiSkillDeliveryFailure::UserInvocationDisabled,
            F::ModelInvocationDisabled => UiSkillDeliveryFailure::ModelInvocationDisabled,
            F::Read => UiSkillDeliveryFailure::Read,
            F::Shadowed => UiSkillDeliveryFailure::Shadowed,
            F::CapabilityUnavailable => UiSkillDeliveryFailure::CapabilityUnavailable,
        }
    }
    UiSkillDelivery {
        entries: audit
            .entries
            .iter()
            .map(|entry| UiSkillDeliveryEntry {
                name: entry.name.clone(),
                requested_as: entry.requested_as.clone(),
                scope: entry.scope.map(ui_scope),
                source: entry.source.map(ui_source),
                bytes: entry.bytes,
                failure: entry.failure.map(failure),
            })
            .collect(),
        blocked: audit.blocked.map(failure),
    }
}

/// Write the body-free audit to the runtime log. The full per-turn record is
/// debug; a refusal or host block is actionable and also logged at info. Only
/// names, the tokens the user typed, provenance, sizes, and stable codes are
/// written — never a procedure body or a filesystem location.
pub(crate) fn trace_delivery(audit: &leveler_skills::SkillDeliveryAudit) {
    tracing::debug!(
        target: "leveler::skills",
        surface = ?audit.surface,
        discovered = audit.discovered,
        index_bytes = audit.index_bytes,
        injected_bytes = audit.injected_bytes,
        blocked = audit.blocked.map(|failure| failure.as_str()).unwrap_or("none"),
        entries = %delivery_summary(audit),
        "skill delivery audit"
    );
    for entry in &audit.entries {
        if let Some(failure) = entry.failure {
            tracing::info!(
                target: "leveler::skills",
                skill = %entry.name,
                requested_as = %entry.requested_as,
                scope = ?entry.scope,
                source = ?entry.source,
                failure = failure.as_str(),
                "skill delivery refused"
            );
        }
    }
}

fn delivery_summary(audit: &leveler_skills::SkillDeliveryAudit) -> String {
    if audit.entries.is_empty() {
        return "-".to_string();
    }
    audit
        .entries
        .iter()
        .map(|entry| match entry.failure {
            None => format!("{}={}B", entry.requested_as, entry.bytes),
            Some(failure) => format!("{}={}", entry.requested_as, failure.as_str()),
        })
        .collect::<Vec<_>>()
        .join(",")
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
        SkillState::Disabled { reason } => (UiSkillStatus::Disabled, Some(reason.clone())),
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
        argument_hint: entry.argument_hint.clone(),
        user_invocable: entry.user_invocable,
        disable_model_invocation: entry.disable_model_invocation,
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
