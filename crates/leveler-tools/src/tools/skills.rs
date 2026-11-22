//! `load_skill` — read a skill's full instructions.
//!
//! Authoring (`save_skill` / `delete_skill`) belongs to the top-level agent and
//! always needs a human's confirmation, like an agent definition: a skill
//! changes what future sessions do. The proposals are validated before anyone
//! is asked, and written through the one [`leveler_skills::SkillStore`] path.

use async_trait::async_trait;
use schemars::JsonSchema;
use serde::Deserialize;
use tokio_util::sync::CancellationToken;

use leveler_execution::RiskLevel;
use leveler_skills::{
    SkillDraft, SkillEntry, SkillFile, SkillRegistry, SkillRoots, SkillScope, SkillStore,
};

use crate::tool::{Tool, ToolContext, ToolError, ToolOutput};

pub const SAVE_SKILL_TOOL: &str = "save_skill";
pub const DELETE_SKILL_TOOL: &str = "delete_skill";

/// Tools that write a skill definition and therefore always need a human's
/// confirmation. A delegated child never holds them.
pub const SKILL_AUTHORING_TOOLS: &[&str] = &[SAVE_SKILL_TOOL, DELETE_SKILL_TOOL];

#[derive(Debug, Deserialize, JsonSchema)]
struct LoadInput {
    /// The skill name (as listed in the skills index).
    name: String,
}

pub struct LoadSkillTool;

#[async_trait]
impl Tool for LoadSkillTool {
    fn name(&self) -> &'static str {
        "load_skill"
    }
    fn description(&self) -> &'static str {
        "Read a skill's full instructions by name (from the injected skills \
         index). Do this before starting work the skill covers."
    }
    fn input_schema(&self) -> serde_json::Value {
        super::schema_of::<LoadInput>()
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
        let input: LoadInput = super::parse_input(self.name(), input)?;
        // First the turn's frozen view (which carries the session allowances
        // and any test admission). If that refuses, ask the CURRENT persistent
        // store before prompting: a human approval made through `/trust` while
        // this turn runs must be honoured, not answered with a duplicate
        // prompt. The frozen view remains the authority for rules, which are
        // already in the prompt.
        let registry = registry_for(&context)?;
        match registry.load_skill(&input.name) {
            Ok(detail) => Ok(ToolOutput::ok(leveler_skills::render_skill_package(
                &detail,
            ))),
            Err(leveler_skills::SkillLookupError::Untrusted { .. }) => {
                let fresh = load_registry_for(&context)?;
                match fresh.load_skill(&input.name) {
                    Ok(detail) => Ok(ToolOutput::ok(leveler_skills::render_skill_package(
                        &detail,
                    ))),
                    // A package that exists but was never admitted is a human
                    // decision, not a typo. Ask the host's approval surface
                    // while this call is still the thing that is waiting; on
                    // approval, reload and continue the original operation.
                    Err(error @ leveler_skills::SkillLookupError::Untrusted { .. }) => {
                        self.authorize_and_load(&input.name, error, &context).await
                    }
                    Err(error) => Ok(ToolOutput::error(error.to_string())),
                }
            }
            Err(error) => Ok(ToolOutput::error(error.to_string())),
        }
    }
}

impl LoadSkillTool {
    async fn authorize_and_load(
        &self,
        name: &str,
        error: leveler_skills::SkillLookupError,
        context: &ToolContext,
    ) -> Result<ToolOutput, ToolError> {
        let Some(package) = error.untrusted_package() else {
            return Ok(ToolOutput::error(error.to_string()));
        };
        let (Some(identity), scope) = (
            package.identity.clone(),
            match package.scope {
                leveler_skills::SkillScope::Project => {
                    leveler_definition_trust::DefinitionScope::Project
                }
                leveler_skills::SkillScope::User => leveler_definition_trust::DefinitionScope::User,
                leveler_skills::SkillScope::Builtin => {
                    return Ok(ToolOutput::error(error.to_string()));
                }
            },
        ) else {
            return Ok(ToolOutput::error(error.to_string()));
        };
        // A store nobody can verify is not a question for the user: an
        // allow-once would still fail the reload, and a persistent approval
        // cannot be written. Say what is actually wrong.
        if matches!(
            package.decision,
            leveler_definition_trust::DefinitionTrust::StoreUnavailable { .. }
        ) {
            return Ok(ToolOutput::error(format!(
                "skill `{name}` is present, but the definition trust store is unavailable ({}), \
                 so no definition can be admitted. Its content was NOT loaded. Repair the store \
                 (see `leveler doctor`) and use /trust.",
                package.state()
            )));
        }
        let Some(authorizer) = context.definition_trust_authorizer().cloned() else {
            return Ok(ToolOutput::error(format!(
                "skill `{name}` is present but not trusted ({}). No human approval surface is \
                 attached to this execution, so the content was NOT loaded. In an interactive \
                 session the user can review it with /trust.",
                package.state()
            )));
        };
        let (current_content, current_digest, content_truncated) =
            match skill_package_preview(&package.location) {
                Ok(preview) => preview,
                Err(error) => {
                    return Ok(ToolOutput::error(format!(
                        "skill `{name}` is present but not trusted, and its current content \
                         could not be read for review ({error}); it was NOT loaded."
                    )));
                }
            };
        // The refusal may be about bytes newer than the error's digest if the
        // file changed between the registry scan and this call. Bind the
        // request to what was actually read now, and refuse if reading moved
        // the identity away from the five-line package the scan refused.
        let approved_content = package
            .decision
            .approved_digest()
            .and_then(|digest| {
                let home = leveler_core::LevelerHome::resolve(&context.execution.environment);
                leveler_definition_trust::read_snapshot(home.root(), digest)
            })
            .map(|bytes| bounded_preview(&String::from_utf8_lossy(&bytes)));
        let request = crate::definition_trust::DefinitionTrustAuthorizationRequest {
            call_id: context
                .tool_call_scope
                .as_deref()
                .unwrap_or("load_skill")
                .to_string(),
            identity,
            kind: leveler_definition_trust::DefinitionKind::Skill,
            scope,
            source: package.source.label().to_string(),
            location: package.location.clone(),
            state: package.decision.clone(),
            current_digest,
            current_content,
            approved_content,
            content_truncated,
        };
        let outcome = authorizer.authorize(request).await;
        match outcome {
            crate::definition_trust::DefinitionTrustAuthorizationOutcome::Denied => {
                Ok(ToolOutput::error(format!(
                    "the user declined to admit skill `{name}`. Its content was NOT loaded and \
                     no trust record was written. Continue without it."
                )))
            }
            crate::definition_trust::DefinitionTrustAuthorizationOutcome::ContentChanged => {
                Ok(ToolOutput::error(format!(
                    "skill `{name}` changed while the approval was being decided, so the \
                     decision did not cover the current bytes. It was NOT loaded; the user can \
                     review the current version with /trust."
                )))
            }
            crate::definition_trust::DefinitionTrustAuthorizationOutcome::Unavailable => {
                Ok(ToolOutput::error(format!(
                    "skill `{name}` is present but not trusted ({}), and this execution cannot \
                     ask a human to admit it (FullAccess without a trusted session baseline, or \
                     no attached client). It was NOT loaded; use /trust to review it.",
                    package.state()
                )))
            }
            crate::definition_trust::DefinitionTrustAuthorizationOutcome::Failed(reason) => {
                Ok(ToolOutput::error(format!(
                    "the approval for skill `{name}` could not be recorded ({reason}). Its \
                     content was NOT loaded; the user can review it with /trust."
                )))
            }
            crate::definition_trust::DefinitionTrustAuthorizationOutcome::AllowedOnce
            | crate::definition_trust::DefinitionTrustAuthorizationOutcome::Trusted => {
                // Re-read through a fresh view. If the bytes moved after the
                // human decision, this refuses instead of handing out content
                // the decision never covered.
                let registry = load_registry_for(context)?;
                match registry.load_skill(name) {
                    Ok(detail) => Ok(ToolOutput::ok(leveler_skills::render_skill_package(
                        &detail,
                    ))),
                    Err(error) => Ok(ToolOutput::error(format!(
                        "skill `{name}` was admitted, but its content could not be verified \
                         afterwards and was NOT loaded: {error}"
                    ))),
                }
            }
        }
    }
}

/// Frame the exact package for display and report its digest. The preview is
/// bounded; the digest is always over the complete package.
fn skill_package_preview(location: &std::path::Path) -> Result<(String, String, bool), String> {
    let package = leveler_definition_trust::read_skill_package(&location.join("SKILL.md"))?;
    let digest = leveler_definition_trust::DefinitionIdentity::skill_package_digest(&package);
    let mut out = String::new();
    let mut truncated = false;
    for part in package {
        let framed = format!(
            "--- {} ---\n{}{}",
            part.relative,
            String::from_utf8_lossy(&part.bytes),
            if part.bytes.ends_with(b"\n") {
                ""
            } else {
                "\n"
            }
        );
        let remaining =
            crate::definition_trust::MAX_AUTHORIZATION_PREVIEW_BYTES.saturating_sub(out.len());
        if framed.len() > remaining {
            truncated = true;
            let mut end = remaining.min(framed.len());
            while !framed.is_char_boundary(end) {
                end -= 1;
            }
            out.push_str(&framed[..end]);
            out.push_str("\n… (preview truncated; the approval binds every package file)\n");
            break;
        }
        out.push_str(&framed);
    }
    Ok((out, digest, truncated))
}

fn bounded_preview(text: &str) -> String {
    if text.len() <= crate::definition_trust::MAX_AUTHORIZATION_PREVIEW_BYTES {
        return text.to_string();
    }
    let mut end = crate::definition_trust::MAX_AUTHORIZATION_PREVIEW_BYTES;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}\n… (preview truncated)\n", &text[..end])
}

/// The scope a client may write a skill into. Mirrors the client protocol's
/// shape without depending on it.
#[derive(Debug, Clone, Copy, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum ScopeArg {
    /// This repository's `.leveler/skills/`, shared through git.
    Project,
    /// The user's `~/.leveler/skills/`, available in every project.
    User,
}

impl ScopeArg {
    fn scope(self) -> SkillScope {
        match self {
            Self::Project => SkillScope::Project,
            Self::User => SkillScope::User,
        }
    }

    fn as_str(self) -> &'static str {
        match self {
            Self::Project => "project",
            Self::User => "user",
        }
    }
}

#[derive(Debug, Clone, Copy, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum ActionArg {
    Create,
    Update,
}

impl ActionArg {
    fn as_str(self) -> &'static str {
        match self {
            Self::Create => "Create",
            Self::Update => "Update",
        }
    }
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct SkillFileArg {
    /// Path relative to the skill directory, e.g. `references/checklist.md`.
    path: String,
    /// The file's contents.
    content: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct SaveSkillArgs {
    /// `project` (this repository) or `user` (every project).
    scope: ScopeArg,
    /// `create` a new skill or `update` an existing one in `scope`.
    action: ActionArg,
    /// Lowercase letters, digits and `-`, starting with a letter; the directory
    /// name is the canonical skill name.
    name: String,
    /// One line the model reads when deciding to use the skill: say when to use
    /// it, with the words a user would type.
    description: String,
    /// The `SKILL.md` Markdown body (the procedure).
    body: String,
    /// Bundled files (scripts, references) written beside `SKILL.md`.
    #[serde(default)]
    files: Vec<SkillFileArg>,
}

impl SaveSkillArgs {
    fn draft(&self) -> SkillDraft {
        SkillDraft {
            name: self.name.trim().to_string(),
            description: self.description.trim().to_string(),
            body: self.body.clone(),
            files: self
                .files
                .iter()
                .map(|f| SkillFile {
                    path: f.path.clone(),
                    content: f.content.clone(),
                })
                .collect(),
        }
    }
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct DeleteSkillArgs {
    /// `project` (this repository) or `user` (every project).
    scope: ScopeArg,
    name: String,
}

/// Whether `tool` writes a skill definition and therefore always needs a
/// human's confirmation.
pub fn is_skill_definition_write(tool: &str) -> bool {
    matches!(tool, SAVE_SKILL_TOOL | DELETE_SKILL_TOOL)
}

fn store_for(context: &ToolContext) -> Result<SkillStore, ToolError> {
    let root = context.require_workspace()?.root();
    Ok(SkillStore::for_project_in(root, &|key| {
        context.execution.environment.var_os(key)
    }))
}

fn registry_for(context: &ToolContext) -> Result<SkillRegistry, ToolError> {
    let root = context.require_workspace()?.root();
    Ok(SkillRegistry::load(
        &SkillRoots::for_project_in(root, &|key| context.execution.environment.var_os(key)),
        context.execution.definition_trust.as_ref(),
    ))
}

/// The registry `load_skill` uses: a fresh store read plus this execution's
/// session allowances. The persistent store is the human authority; the turn's
/// frozen view is the authority for everything that must not change mid-turn.
fn load_registry_for(context: &ToolContext) -> Result<SkillRegistry, ToolError> {
    let root = context.require_workspace()?.root();
    let home = leveler_core::LevelerHome::resolve(&context.execution.environment);
    let mut trust = leveler_definition_trust::DefinitionTrustView::load(home.root(), Some(root));
    if let Some(session) = context.execution.definition_trust.session() {
        trust = trust.with_session(session.clone());
    }
    Ok(SkillRegistry::load(
        &SkillRoots::for_project_in(root, &|key| context.execution.environment.var_os(key)),
        &trust,
    ))
}

/// Validate a `save_skill` / `delete_skill` call before anyone is asked, and
/// render what the approval prompt shows. `Err` is the refusal the model gets.
pub fn skill_authoring_preflight(
    tool: &str,
    arguments: &serde_json::Value,
    context: &ToolContext,
) -> Result<String, String> {
    let store = store_for(context).map_err(|e| e.to_string())?;
    let registry = registry_for(context).map_err(|e| e.to_string())?;
    match tool {
        SAVE_SKILL_TOOL => {
            let args: SaveSkillArgs = serde_json::from_value(arguments.clone())
                .map_err(|e| format!("save_skill: {e}"))?;
            save_preview(
                &args,
                &store,
                &registry,
                context
                    .require_workspace()
                    .map_err(|e| e.to_string())?
                    .root(),
            )
        }
        DELETE_SKILL_TOOL => {
            let args: DeleteSkillArgs = serde_json::from_value(arguments.clone())
                .map_err(|e| format!("delete_skill: {e}"))?;
            let name = args.name.trim();
            let scope = args.scope.scope();
            let target = store
                .validate(
                    scope,
                    &SkillDraft {
                        name: name.to_string(),
                        description: "x".into(),
                        body: "x".into(),
                        files: Vec::new(),
                    },
                )
                .map_err(|e| e.to_string())?;
            if !target.is_dir() {
                return Err(delete_refusal(name, scope.as_str(), &registry));
            }
            Ok(format!(
                "Delete {scope} skill \"{name}\"\nLocation: {location}\n\
                 Future turns will no longer resolve this name here.",
                scope = args.scope.as_str(),
                location = display_path(
                    &target,
                    scope,
                    context
                        .require_workspace()
                        .map_err(|e| e.to_string())?
                        .root()
                ),
            ))
        }
        _ => Err(format!("{tool} is not a skill authoring tool")),
    }
}

/// Why a delete was refused: the name may still be usable *somewhere*, and
/// saying where saves the user from guessing.
fn delete_refusal(name: &str, scope: &str, registry: &SkillRegistry) -> String {
    match registry.get(name) {
        Some(entry) => format!(
            "no CodeLeveler-managed {scope} skill \"{name}\"; the active skill of that name \
             comes from {} and is not managed here",
            entry.source.label()
        ),
        None => format!("no {scope} skill \"{name}\" exists"),
    }
}

fn display_path(target: &std::path::Path, scope: SkillScope, root: &std::path::Path) -> String {
    if scope == SkillScope::Project
        && let Ok(relative) = target.strip_prefix(root)
    {
        return relative.display().to_string();
    }
    target.display().to_string()
}

fn save_preview(
    args: &SaveSkillArgs,
    store: &SkillStore,
    registry: &SkillRegistry,
    workspace: &std::path::Path,
) -> Result<String, String> {
    let draft = args.draft();
    let scope = args.scope.scope();
    let target = store.validate(scope, &draft).map_err(|e| e.to_string())?;
    let name = draft.name.as_str();
    let exists = target.is_dir();
    match (args.action, exists) {
        (ActionArg::Create, true) => {
            return Err(format!(
                "{} skill \"{name}\" already exists; use action=update to change it",
                args.scope.as_str()
            ));
        }
        (ActionArg::Update, false) => {
            return Err(format!(
                "{} skill \"{name}\" does not exist{}",
                args.scope.as_str(),
                match registry.get(name) {
                    Some(entry) => format!(
                        " (the active \"{name}\" is a {} skill, which cannot be updated here)",
                        entry.source.label()
                    ),
                    None => String::new(),
                }
            ));
        }
        _ => {}
    }
    // A native create may legitimately shadow a lower-precedence skill; say so.
    let shadow_note = registry
        .get(name)
        .filter(|entry| entry.source != leveler_skills::SkillSource::Native)
        .map(|entry| {
            format!(
                "\nShadows\n  the {} skill of the same name, which stays installed\n",
                entry.source.label()
            )
        })
        .unwrap_or_default();
    let mut files = vec!["SKILL.md".to_string()];
    for file in &draft.files {
        files.push(file.path.clone());
    }
    Ok(format!(
        "{verb} Skill\n\n\
         Name\n  {name}\n\n\
         Description\n  {description}\n\n\
         Scope\n  {scope}\n\n\
         Location\n  {location}\n\n\
         Files\n  {files}\n{shadow}",
        verb = args.action.as_str(),
        description = draft.description,
        scope = args.scope.as_str(),
        location = display_path(&target, scope, workspace),
        files = files.join("\n  "),
        shadow = shadow_note,
    ))
}

/// Whether the resolved entry is usable, as one line for a tool result.
fn status_line(entry: &SkillEntry) -> String {
    match &entry.state {
        leveler_skills::SkillState::Available => "available".to_string(),
        leveler_skills::SkillState::Invalid(reason) => format!("invalid: {reason}"),
    }
}

pub struct SaveSkillTool;

#[async_trait]
impl Tool for SaveSkillTool {
    fn name(&self) -> &'static str {
        SAVE_SKILL_TOOL
    }

    fn description(&self) -> &'static str {
        "Create or update a reusable skill (SKILL.md, plus optional bundled \
         scripts and references) when the user asks to save or improve one. The \
         user always confirms the exact proposal before anything is written. \
         Choose `scope` from what the user said — this project, or all their \
         projects — and ask if they did not say. To edit a skill discovered from \
         another tool, copy its content into a new native skill under a \
         different name instead."
    }

    fn input_schema(&self) -> serde_json::Value {
        super::schema_of::<SaveSkillArgs>()
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
        let args: SaveSkillArgs = super::parse_input(self.name(), input)?;
        let store = store_for(&context)?;
        let draft = args.draft();
        let scope = args.scope.scope();
        let written = match args.action {
            ActionArg::Create => store.create(scope, &draft),
            ActionArg::Update => store.update(scope, &draft),
        };
        let dir = match written {
            Ok(dir) => dir,
            Err(error) => return Ok(ToolOutput::error(error.to_string())),
        };
        // Re-resolve so the result reports the registry's real state, not the
        // intent.
        let registry = registry_for(&context)?;
        let status = registry
            .get(&draft.name)
            .map(status_line)
            .unwrap_or_else(|| "not visible in the registry".to_string());
        Ok(ToolOutput::ok(format!(
            "Saved {} skill \"{}\" at {} ({}). New turns resolve it by name.",
            args.scope.as_str(),
            draft.name,
            dir.display(),
            status
        )))
    }
}

pub struct DeleteSkillTool;

#[async_trait]
impl Tool for DeleteSkillTool {
    fn name(&self) -> &'static str {
        DELETE_SKILL_TOOL
    }

    fn description(&self) -> &'static str {
        "Delete a project or user skill CodeLeveler manages, when the user asks \
         and confirms. Built-in skills and skills installed for other tools \
         (Codex, Claude Code, Agent Skills) cannot be deleted here."
    }

    fn input_schema(&self) -> serde_json::Value {
        super::schema_of::<DeleteSkillArgs>()
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
        let args: DeleteSkillArgs = super::parse_input(self.name(), input)?;
        let name = args.name.trim();
        let scope = args.scope.scope();
        match store_for(&context)?.delete(scope, name) {
            Ok(()) => Ok(ToolOutput::ok(format!(
                "Deleted {} skill \"{name}\".",
                args.scope.as_str()
            ))),
            Err(error) => {
                let registry = registry_for(&context)?;
                if registry.get(name).is_some() {
                    return Ok(ToolOutput::error(delete_refusal(
                        name,
                        args.scope.as_str(),
                        &registry,
                    )));
                }
                Ok(ToolOutput::error(error.to_string()))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn context(root: &std::path::Path) -> ToolContext {
        let ws = leveler_execution::Workspace::new(root).unwrap();
        ToolContext::new(ws, leveler_execution::PermissionProfile::Assisted)
            .trust_all_definitions_for_tests()
    }

    fn tmp(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "leveler-skills-{tag}-{}-{}",
            std::process::id(),
            super::super::test_ordinal()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// Skill content is data: reading it is read-only and confers no new
    /// capability. Permissions, sandbox and write scope stay runtime-enforced.
    #[test]
    fn loading_a_skill_is_read_only() {
        assert_eq!(Tool::risk(&LoadSkillTool), RiskLevel::Safe);
        assert_eq!(Tool::name(&LoadSkillTool), "load_skill");
    }

    #[tokio::test]
    async fn load_skill_surfaces_structured_scripts_and_dir() {
        let dir = tmp("struct");
        let ctx = context(&dir);
        let store = store_for(&ctx).unwrap();
        store
            .create(
                SkillScope::Project,
                &SkillDraft {
                    name: "pack".into(),
                    description: "Pack things".into(),
                    body: "UNIQUE_PACK_BODY_99".into(),
                    files: vec![
                        SkillFile {
                            path: "scripts/run.sh".into(),
                            content: "echo run\n".into(),
                        },
                        SkillFile {
                            path: "references/a.md".into(),
                            content: "ref\n".into(),
                        },
                    ],
                },
            )
            .unwrap();
        let root = ctx.require_workspace().unwrap().root().to_path_buf();
        let skill_dir = root.join(".leveler").join("skills").join("pack");

        let loaded = LoadSkillTool
            .execute(
                serde_json::json!({ "name": "pack" }),
                ctx,
                CancellationToken::new(),
            )
            .await
            .unwrap();
        assert!(!loaded.is_error, "{}", loaded.content);
        assert!(loaded.content.contains("UNIQUE_PACK_BODY_99"));
        assert!(loaded.content.contains("## Scripts"));
        assert!(loaded.content.contains("scripts/run.sh"));
        assert!(loaded.content.contains("## References"));
        assert!(loaded.content.contains("references/a.md"));
        assert!(
            loaded
                .content
                .contains(skill_dir.to_string_lossy().as_ref())
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn load_missing_skill_is_error() {
        let dir = tmp("miss");
        let ctx = context(&dir);
        let out = LoadSkillTool
            .execute(
                serde_json::json!({ "name": "nope" }),
                ctx,
                CancellationToken::new(),
            )
            .await
            .unwrap();
        assert!(out.is_error);
        assert!(out.content.contains("no skill named"), "{}", out.content);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn save_preflight_shows_the_proposal_not_a_file_write() {
        let dir = tmp("preflight");
        let ctx = context(&dir);
        let description = skill_authoring_preflight(
            SAVE_SKILL_TOOL,
            &serde_json::json!({
                "scope": "project",
                "action": "create",
                "name": "windows-ci-debug",
                "description": "Diagnose intermittent Windows CI failures.",
                "body": "1. Reproduce.",
                "files": [{ "path": "references/checklist.md", "content": "x" }],
            }),
            &ctx,
        )
        .unwrap();
        assert!(description.contains("Create Skill"), "{description}");
        assert!(description.contains("windows-ci-debug"), "{description}");
        assert!(
            description.contains("references/checklist.md"),
            "{description}"
        );
        // The location is host-native (backslashes on Windows); compare on a
        // separator-normalized copy so the assertion tests the path, not the OS.
        assert!(
            description.replace('\\', "/").contains(".leveler/skills"),
            "{description}"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn save_preflight_refuses_a_duplicate_create_and_a_missing_update() {
        let dir = tmp("preflight2");
        let ctx = context(&dir);
        let args = serde_json::json!({
            "scope": "project",
            "action": "create",
            "name": "dup",
            "description": "d",
            "body": "b",
        });
        store_for(&ctx)
            .unwrap()
            .create(
                SkillScope::Project,
                &SkillDraft {
                    name: "dup".into(),
                    description: "d".into(),
                    body: "b".into(),
                    files: Vec::new(),
                },
            )
            .unwrap();
        let error = skill_authoring_preflight(SAVE_SKILL_TOOL, &args, &ctx).unwrap_err();
        assert!(error.contains("already exists"), "{error}");
        let error = skill_authoring_preflight(
            SAVE_SKILL_TOOL,
            &serde_json::json!({
                "scope": "project",
                "action": "update",
                "name": "absent",
                "description": "d",
                "body": "b",
            }),
            &ctx,
        )
        .unwrap_err();
        assert!(error.contains("does not exist"), "{error}");
        std::fs::remove_dir_all(&dir).ok();
    }
}

#[cfg(test)]
mod trust_tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    use async_trait::async_trait;
    use leveler_definition_trust::{
        DefinitionTrustView, SessionDefinitionTrust, SessionTrustSource,
    };

    struct FakeAuthorizer {
        session: Arc<SessionDefinitionTrust>,
        decision: crate::definition_trust::DefinitionTrustAuthorizationOutcome,
        seen: Mutex<Vec<crate::definition_trust::DefinitionTrustAuthorizationRequest>>,
        mutate: Option<(std::path::PathBuf, String)>,
    }

    #[async_trait]
    impl crate::definition_trust::DefinitionTrustAuthorizer for FakeAuthorizer {
        async fn authorize(
            &self,
            request: crate::definition_trust::DefinitionTrustAuthorizationRequest,
        ) -> crate::definition_trust::DefinitionTrustAuthorizationOutcome {
            self.seen.lock().unwrap().push(request.clone());
            if let Some((path, body)) = &self.mutate {
                std::fs::write(path, body).unwrap();
            }
            match self.decision.clone() {
                crate::definition_trust::DefinitionTrustAuthorizationOutcome::AllowedOnce => {
                    self.session.allow(
                        &request.identity,
                        &request.current_digest,
                        SessionTrustSource::AllowOnce,
                    );
                    crate::definition_trust::DefinitionTrustAuthorizationOutcome::AllowedOnce
                }
                other => other,
            }
        }
    }

    fn untrusted_context(
        root: &std::path::Path,
        home: &std::path::Path,
        authorizer: Option<Arc<dyn crate::definition_trust::DefinitionTrustAuthorizer>>,
        session: &Arc<SessionDefinitionTrust>,
    ) -> ToolContext {
        // The tool resolves the live store from its own environment snapshot.
        // Point that snapshot at the test home so the frozen view and the
        // fresh store read name the same store, as they do in production.
        let env = leveler_core::EnvSnapshot::new(
            [(
                std::ffi::OsString::from("LEVELER_HOME"),
                home.as_os_str().to_os_string(),
            )],
            root.to_path_buf(),
            std::env::temp_dir(),
        );
        let mut ctx = ToolContext::with_environment(
            leveler_execution::Workspace::new(root).unwrap(),
            leveler_execution::PermissionProfile::Assisted,
            Arc::new(env),
        );
        let view = DefinitionTrustView::load(home, Some(root)).with_session(session.clone());
        ctx.execution.definition_trust = Arc::new(view);
        ctx.execution.definition_trust_authorizer = authorizer;
        ctx
    }

    fn write_evil(root: &std::path::Path) -> std::path::PathBuf {
        let dir = root.join(".leveler/skills/evil");
        std::fs::create_dir_all(&dir).unwrap();
        let skill = dir.join("SKILL.md");
        std::fs::write(
            &skill,
            "---\nname: evil\ndescription: Evil.\n---\n\nEVIL_SKILL_BODY\n",
        )
        .unwrap();
        skill
    }

    #[tokio::test]
    async fn an_unapproved_skill_body_is_not_returned() {
        let dir = tempfile::tempdir().unwrap();
        let home = tempfile::tempdir().unwrap();
        write_evil(dir.path());
        let session = Arc::new(SessionDefinitionTrust::unknown());
        let ctx = untrusted_context(dir.path(), home.path(), None, &session);
        let out = LoadSkillTool
            .execute(
                serde_json::json!({"name": "evil"}),
                ctx,
                CancellationToken::new(),
            )
            .await
            .unwrap();
        assert!(out.is_error, "{}", out.content);
        assert!(
            !out.content.contains("EVIL_SKILL_BODY"),
            "unapproved skill body leaked: {}",
            out.content
        );
        assert!(
            out.content.contains("/trust") || out.content.contains("not trusted"),
            "{}",
            out.content
        );
    }

    #[tokio::test]
    async fn an_allow_once_decision_resumes_the_same_load() {
        let dir = tempfile::tempdir().unwrap();
        let home = tempfile::tempdir().unwrap();
        write_evil(dir.path());
        let session = Arc::new(SessionDefinitionTrust::unknown());
        let authorizer = Arc::new(FakeAuthorizer {
            session: session.clone(),
            decision: crate::definition_trust::DefinitionTrustAuthorizationOutcome::AllowedOnce,
            seen: Mutex::new(Vec::new()),
            mutate: None,
        });
        let ctx = untrusted_context(dir.path(), home.path(), Some(authorizer.clone()), &session);
        let out = LoadSkillTool
            .execute(
                serde_json::json!({"name": "evil"}),
                ctx,
                CancellationToken::new(),
            )
            .await
            .unwrap();
        assert!(!out.is_error, "{}", out.content);
        assert!(out.content.contains("EVIL_SKILL_BODY"), "{}", out.content);
        let seen = authorizer.seen.lock().unwrap();
        assert_eq!(seen.len(), 1);
        assert_eq!(
            seen[0].identity.kind,
            leveler_definition_trust::DefinitionKind::Skill
        );
        assert!(seen[0].current_content.contains("EVIL_SKILL_BODY"));
        assert!(seen[0].current_digest.starts_with("sha256:"));
        // Nothing was written to the persistent store by an allow-once.
        assert_eq!(
            leveler_definition_trust::DefinitionTrustStore::load(home.path()).status(),
            &leveler_definition_trust::StoreStatus::Missing
        );
    }

    #[tokio::test]
    async fn a_denied_decision_returns_a_refusal_without_the_body() {
        let dir = tempfile::tempdir().unwrap();
        let home = tempfile::tempdir().unwrap();
        write_evil(dir.path());
        let session = Arc::new(SessionDefinitionTrust::unknown());
        let authorizer = Arc::new(FakeAuthorizer {
            session: session.clone(),
            decision: crate::definition_trust::DefinitionTrustAuthorizationOutcome::Denied,
            seen: Mutex::new(Vec::new()),
            mutate: None,
        });
        let ctx = untrusted_context(dir.path(), home.path(), Some(authorizer), &session);
        let out = LoadSkillTool
            .execute(
                serde_json::json!({"name": "evil"}),
                ctx,
                CancellationToken::new(),
            )
            .await
            .unwrap();
        assert!(out.is_error);
        assert!(!out.content.contains("EVIL_SKILL_BODY"));
        assert!(out.content.contains("declined"), "{}", out.content);
    }

    #[tokio::test]
    async fn content_changed_under_the_decision_is_not_loaded() {
        let dir = tempfile::tempdir().unwrap();
        let home = tempfile::tempdir().unwrap();
        let skill = write_evil(dir.path());
        let session = Arc::new(SessionDefinitionTrust::unknown());
        let authorizer = Arc::new(FakeAuthorizer {
            session: session.clone(),
            decision: crate::definition_trust::DefinitionTrustAuthorizationOutcome::AllowedOnce,
            seen: Mutex::new(Vec::new()),
            mutate: Some((
                skill,
                "---\nname: evil\ndescription: Evil.\n---\n\nREPLACED_BODY\n".into(),
            )),
        });
        let ctx = untrusted_context(dir.path(), home.path(), Some(authorizer), &session);
        let out = LoadSkillTool
            .execute(
                serde_json::json!({"name": "evil"}),
                ctx,
                CancellationToken::new(),
            )
            .await
            .unwrap();
        assert!(out.is_error, "{}", out.content);
        assert!(!out.content.contains("EVIL_SKILL_BODY"));
        assert!(!out.content.contains("REPLACED_BODY"));
        assert!(
            out.content.contains("verified") || out.content.contains("could not"),
            "{}",
            out.content
        );
    }

    #[tokio::test]
    async fn a_trust_page_approval_during_a_turn_is_honoured_without_a_second_prompt() {
        let dir = tempfile::tempdir().unwrap();
        let home = tempfile::tempdir().unwrap();
        write_evil(dir.path());
        let session = Arc::new(SessionDefinitionTrust::unknown());
        // The turn started before the approval: its frozen view refuses.
        let ctx = untrusted_context(dir.path(), home.path(), None, &session);
        let frozen = (*ctx.execution.definition_trust).clone();
        let candidate = leveler_definition_trust::discover_project(dir.path())
            .into_iter()
            .find(|candidate| candidate.identity.display.contains(".leveler/skills/evil"))
            .unwrap();
        assert!(
            !frozen
                .check(&candidate.identity, candidate.digest.as_deref().unwrap())
                .admits()
        );
        // `/trust` writes the exact bytes mid-turn.
        leveler_definition_trust::approve(
            home.path(),
            dir.path(),
            &[candidate.approval_request().unwrap()],
            leveler_definition_trust::ApprovalSurface::Tui,
        )
        .unwrap();
        let out = LoadSkillTool
            .execute(
                serde_json::json!({"name": "evil"}),
                ctx,
                CancellationToken::new(),
            )
            .await
            .unwrap();
        assert!(
            !out.is_error,
            "the persistent approval must be read at load time: {}",
            out.content
        );
        assert!(out.content.contains("EVIL_SKILL_BODY"));
    }

    #[tokio::test]
    async fn a_corrupt_store_is_reported_without_a_prompt_or_body() {
        let dir = tempfile::tempdir().unwrap();
        let home = tempfile::tempdir().unwrap();
        write_evil(dir.path());
        std::fs::write(
            leveler_definition_trust::store_path(home.path()),
            "!! not yaml !!",
        )
        .unwrap();
        let session = Arc::new(SessionDefinitionTrust::unknown());
        let authorizer = Arc::new(FakeAuthorizer {
            session: session.clone(),
            decision: crate::definition_trust::DefinitionTrustAuthorizationOutcome::AllowedOnce,
            seen: Mutex::new(Vec::new()),
            mutate: None,
        });
        let ctx = untrusted_context(dir.path(), home.path(), Some(authorizer.clone()), &session);
        let out = LoadSkillTool
            .execute(
                serde_json::json!({"name": "evil"}),
                ctx,
                CancellationToken::new(),
            )
            .await
            .unwrap();
        assert!(out.is_error, "{}", out.content);
        assert!(!out.content.contains("EVIL_SKILL_BODY"));
        assert!(out.content.contains("unavailable"), "{}", out.content);
        assert!(
            authorizer.seen.lock().unwrap().is_empty(),
            "a corrupt store must not raise an approval prompt"
        );
    }

    #[tokio::test]
    async fn an_unapproved_skill_body_is_not_returned_with_deny_all() {
        let dir = tempfile::tempdir().unwrap();
        let skill = dir.path().join(".leveler/skills/evil");
        std::fs::create_dir_all(&skill).unwrap();
        std::fs::write(
            skill.join("SKILL.md"),
            "---\nname: evil\ndescription: Evil.\n---\n\nEVIL_SKILL_BODY\n",
        )
        .unwrap();
        let ws = leveler_execution::Workspace::new(dir.path()).unwrap();
        let mut ctx = ToolContext::new(ws, leveler_execution::PermissionProfile::Assisted);
        ctx.execution.definition_trust =
            std::sync::Arc::new(leveler_definition_trust::DefinitionTrustView::deny_all());
        let out = LoadSkillTool
            .execute(
                serde_json::json!({"name": "evil"}),
                ctx,
                CancellationToken::new(),
            )
            .await
            .unwrap();
        assert!(out.is_error, "{}", out.content);
        assert!(
            !out.content.contains("EVIL_SKILL_BODY"),
            "unapproved skill body leaked: {}",
            out.content
        );
    }
}
