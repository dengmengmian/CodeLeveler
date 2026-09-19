//! Agent Skills: discover, resolve, load, and author `SKILL.md`
//! procedural-knowledge packs.
//!
//! Two sides, one identity:
//!
//! - The **read side** is [`SkillRegistry`]. It scans the known roots once,
//!   resolves names by locality (project, then user, then built-in) and exposes
//!   one answer for the context index, `$name` mentions, `load_skill`,
//!   `Agent.skills`, the CLI and the TUI. Every consumer reads it; none of them
//!   re-walks the filesystem on its own.
//! - The **write side** is [`SkillStore`]. Only CodeLeveler's own project and
//!   user stores are written, only after a validated proposal and a human's
//!   confirmation, and always atomically.
//!
//! The directory name is the canonical skill name. A frontmatter `name` that
//! disagrees with it is an invalid package, so discovery and loading can never
//! disagree about what a skill is called.
//!
//! Compatible skills already installed for other agents (`~/.codex/skills`,
//! `~/.agents/skills`, `~/.claude/skills`, and their project-local siblings) are
//! read where they live — never copied — so another tool's update is visible on
//! the next resolve.
#![forbid(unsafe_code)]

mod builtin;
mod frontmatter;
mod name;
mod registry;
mod render;
mod store;

pub use builtin::is_builtin_name;
pub use name::{MAX_SKILL_NAME_LEN, is_mentionable, validate_read_name, validate_skill_name};
pub use registry::{
    ShadowedSkill, SkillDetail, SkillEntry, SkillLookupError, SkillMentionResolution, SkillProblem,
    SkillRegistry, SkillRoot, SkillRoots, SkillScope, SkillSource, SkillState, SkillSummary,
    describe, discover, load, resolve_mentions,
};
pub use render::{
    MAX_SKILL_INDEX_BYTES, SKILLS_HOW_TO_USE, parse_skill_mentions, render_capped_index,
    render_index, render_skill_package, render_turn_injection,
};
pub use store::{
    MAX_BODY_BYTES, MAX_BUNDLED_BYTES, MAX_BUNDLED_FILES, MAX_DESCRIPTION_CHARS, MAX_FILE_BYTES,
    SkillDraft, SkillFile, SkillStore, SkillStoreError,
};

use std::path::{Path, PathBuf};

/// The project skills directory for a repo root.
pub fn project_skills_dir(root: &Path) -> PathBuf {
    root.join(".leveler").join("skills")
}

#[cfg(test)]
mod builtin_tests {
    use super::*;

    /// The shipped creator is discoverable and loadable through the same path a
    /// user takes.
    #[test]
    fn skill_creator_ships_and_loads() {
        let root = std::env::temp_dir().join("leveler-builtin-creator-none");
        let registry = describe(&root);
        let entry = registry
            .get("skill-creator")
            .expect("skill-creator must be discovered");
        assert_eq!(entry.scope, SkillScope::Builtin);
        assert_eq!(entry.source, SkillSource::Builtin);
        assert!(entry.is_available(), "{:?}", entry.state);
        let detail = registry.load_skill("skill-creator").expect("loadable");
        assert!(detail.body.contains("Skill Creator"));
        assert!(detail.scripts.is_empty(), "built-ins have no bundled files");
    }

    #[test]
    fn builtin_names_cannot_be_written_through_the_store() {
        let root = std::env::temp_dir().join("leveler-builtin-write-none");
        let store = SkillStore::new(Some(root), None);
        let draft = SkillDraft {
            name: "skill-creator".into(),
            description: "x".into(),
            body: "y".into(),
            files: Vec::new(),
        };
        assert!(store.create(SkillScope::Project, &draft).is_err());
    }
}
