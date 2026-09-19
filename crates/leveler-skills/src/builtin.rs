//! Skills shipped with the binary, projected into the same resolved view as
//! file-based ones. A built-in is discoverable and loadable, and can be
//! shadowed by a project or user skill of the same name, but it has no package
//! directory — so it can never be edited or deleted through the authoring
//! store.
//!
//! Packages live under `builtin/<name>/SKILL.md` and are compiled in, so the
//! runtime depends on no external file and the shipped text is the same text
//! the parser validates.

use std::path::PathBuf;
use std::sync::OnceLock;

use crate::frontmatter;
use crate::registry::{SkillDetail, SkillScope, SkillSource};

pub(crate) struct Builtin {
    pub(crate) name: &'static str,
    pub(crate) description: &'static str,
    content: &'static str,
}

/// Compiled packages, as `(name, SKILL.md)`.
const PACKS: &[(&str, &str)] = &[(
    "skill-creator",
    include_str!("../builtin/skill-creator/SKILL.md"),
)];

/// The parsed built-ins. A package whose own frontmatter is invalid is a build
/// defect the unit tests catch; at runtime it is simply absent.
fn parsed() -> &'static [Builtin] {
    static CACHE: OnceLock<Vec<Builtin>> = OnceLock::new();
    CACHE.get_or_init(|| {
        PACKS
            .iter()
            .filter_map(|(name, content)| {
                let parsed = frontmatter::parse(content).ok()?;
                let description = parsed.frontmatter.description()?;
                // The constant and the manifest must agree, exactly as for a
                // file-based package's directory name.
                if parsed.frontmatter.name() != Some(*name) {
                    return None;
                }
                Some(Builtin {
                    name,
                    description: leak(description),
                    content,
                })
            })
            .collect()
    })
}

/// Intern a runtime string once, for the process-lifetime builtin cache.
fn leak(value: String) -> &'static str {
    Box::leak(value.into_boxed_str())
}

pub(crate) fn skills() -> &'static [Builtin] {
    parsed()
}

/// Whether `name` ships with the binary.
pub fn is_builtin_name(name: &str) -> bool {
    parsed().iter().any(|b| b.name == name)
}

/// The full detail of a built-in skill, or `None` if no such package is
/// compiled in.
pub(crate) fn detail(name: &str) -> Option<SkillDetail> {
    let builtin = parsed().iter().find(|b| b.name == name)?;
    let parsed = frontmatter::parse(builtin.content).ok()?;
    Some(SkillDetail {
        name: builtin.name.to_string(),
        description: builtin.description.to_string(),
        body: parsed.body,
        scope: SkillScope::Builtin,
        source: SkillSource::Builtin,
        dir: PathBuf::from("(built-in)"),
        scripts: Vec::new(),
        references: Vec::new(),
        other_files: Vec::new(),
    })
}
