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
        match registry_for(&context).load_skill(&input.name) {
            Ok(detail) => Ok(ToolOutput::ok(leveler_skills::render_skill_package(
                &detail,
            ))),
            Err(error) => Ok(ToolOutput::error(error.to_string())),
        }
    }
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

fn store_for(context: &ToolContext) -> SkillStore {
    let root = context.execution.workspace.root();
    SkillStore::for_project_in(root, &|key| context.execution.environment.var_os(key))
}

fn registry_for(context: &ToolContext) -> SkillRegistry {
    let root = context.execution.workspace.root();
    SkillRegistry::load(&SkillRoots::for_project_in(root, &|key| {
        context.execution.environment.var_os(key)
    }))
}

/// Validate a `save_skill` / `delete_skill` call before anyone is asked, and
/// render what the approval prompt shows. `Err` is the refusal the model gets.
pub fn skill_authoring_preflight(
    tool: &str,
    arguments: &serde_json::Value,
    context: &ToolContext,
) -> Result<String, String> {
    let store = store_for(context);
    let registry = registry_for(context);
    match tool {
        SAVE_SKILL_TOOL => {
            let args: SaveSkillArgs = serde_json::from_value(arguments.clone())
                .map_err(|e| format!("save_skill: {e}"))?;
            save_preview(&args, &store, &registry, context.execution.workspace.root())
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
                location = display_path(&target, scope, context.execution.workspace.root()),
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
        let store = store_for(&context);
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
        let registry = registry_for(&context);
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
        match store_for(&context).delete(scope, name) {
            Ok(()) => Ok(ToolOutput::ok(format!(
                "Deleted {} skill \"{name}\".",
                args.scope.as_str()
            ))),
            Err(error) => {
                let registry = registry_for(&context);
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
    }

    fn tmp(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "leveler-skills-{tag}-{}",
            super::super::test_ordinal()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[tokio::test]
    async fn load_skill_surfaces_structured_scripts_and_dir() {
        let dir = tmp("struct");
        let ctx = context(&dir);
        let store = store_for(&ctx);
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
        let root = ctx.execution.workspace.root().to_path_buf();
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
