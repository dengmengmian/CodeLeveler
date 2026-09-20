//! Creating, updating and deleting the skills CodeLeveler manages.
//!
//! The one write path every surface uses — `save_skill`, the CLI and a future
//! client command — so validation, path safety and atomicity live in one place.
//! A skill is a directory: `SKILL.md` plus any bundled files. The write is
//! staged in a hidden sibling directory and renamed into place, and the
//! registry ignores hidden entries, so an interrupted mutation leaves an
//! invisible leftover, never a half-written skill.
//!
//! Only CodeLeveler's own project and user stores are written. Skills
//! discovered from Codex, the Agent Skills standard or Claude Code are read and
//! used where they already live, and are never copied or mutated here.

use std::io::Write as _;
use std::path::{Component, Path, PathBuf};

use crate::frontmatter;
use crate::name;
use crate::registry::SkillScope;

/// Longest description CodeLeveler writes into frontmatter.
pub const MAX_DESCRIPTION_CHARS: usize = 1024;
/// Longest `SKILL.md` body CodeLeveler writes.
pub const MAX_BODY_BYTES: usize = 256 * 1024;
/// Longest a single bundled file may be.
pub const MAX_FILE_BYTES: usize = 256 * 1024;
/// Longest the bundled files of one skill may total.
pub const MAX_BUNDLED_BYTES: usize = 1024 * 1024;
/// Most bundled files one skill may declare.
pub const MAX_BUNDLED_FILES: usize = 256;

/// Why a mutation was refused. Nothing was written.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkillStoreError(pub String);

impl std::fmt::Display for SkillStoreError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for SkillStoreError {}

fn fail<T>(message: impl Into<String>) -> Result<T, SkillStoreError> {
    Err(SkillStoreError(message.into()))
}

/// One bundled file, path relative to the skill directory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkillFile {
    pub path: String,
    pub content: String,
}

/// A validated proposal for a skill's contents.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkillDraft {
    pub name: String,
    pub description: String,
    pub body: String,
    pub files: Vec<SkillFile>,
}

/// The single write path for CodeLeveler-managed skills.
pub struct SkillStore {
    project_root: Option<PathBuf>,
    user_skills_dir: Option<PathBuf>,
}

impl SkillStore {
    /// The store for a project, with the user directory resolved from the live
    /// environment.
    pub fn for_project(root: &Path) -> Self {
        Self::for_project_in(root, &|key| leveler_core::environment().var_os(key))
    }

    /// [`Self::for_project`] with an injected environment lookup, for tests.
    pub fn for_project_in<F>(root: &Path, var: &F) -> Self
    where
        F: Fn(&str) -> Option<std::ffi::OsString>,
    {
        let user_skills_dir = leveler_core::leveler_home_dir_from(var)
            .map(|home| leveler_core::LevelerHome::from_root(home).skills_dir());
        Self {
            project_root: Some(root.to_path_buf()),
            user_skills_dir,
        }
    }

    /// Explicit roots, for a caller that owns the home resolution.
    pub fn new(project_root: Option<PathBuf>, user_skills_dir: Option<PathBuf>) -> Self {
        Self {
            project_root,
            user_skills_dir,
        }
    }

    pub fn project_root(&self) -> Option<&Path> {
        self.project_root.as_deref()
    }

    pub fn user_skills_dir(&self) -> Option<&Path> {
        self.user_skills_dir.as_deref()
    }

    /// Create a new skill. Refused if one of that name already exists in the
    /// scope.
    pub fn create(
        &self,
        scope: SkillScope,
        draft: &SkillDraft,
    ) -> Result<PathBuf, SkillStoreError> {
        let name = checked_name(&draft.name)?;
        let skills_dir = self.skills_dir(scope, true)?;
        let target = skills_dir.join(name);
        let skill_md = render_skill_md(draft)?;
        if std::fs::symlink_metadata(&target).is_ok() {
            return fail(format!(
                "{} skill \"{name}\" already exists at {}; update it instead",
                scope.as_str(),
                target.display()
            ));
        }
        let staging = stage(&skills_dir, name, &skill_md, &draft.files)?;
        if let Err(error) = std::fs::rename(&staging, &target) {
            let _ = std::fs::remove_dir_all(&staging);
            return fail(format!("cannot create {}: {error}", target.display()));
        }
        Ok(target)
    }

    /// Replace an existing skill in the scope with the complete draft. Files
    /// not in the draft are removed: the proposal is the skill.
    pub fn update(
        &self,
        scope: SkillScope,
        draft: &SkillDraft,
    ) -> Result<PathBuf, SkillStoreError> {
        let name = checked_name(&draft.name)?;
        let skills_dir = self.skills_dir(scope, false)?;
        let target = skills_dir.join(name);
        existing_directory(&target, scope, name)?;
        let skill_md = render_skill_md(draft)?;
        let staging = stage(&skills_dir, name, &skill_md, &draft.files)?;
        let aside = skills_dir.join(format!(".{name}.old-{}", unique()));
        if let Err(error) = std::fs::rename(&target, &aside) {
            let _ = std::fs::remove_dir_all(&staging);
            return fail(format!("cannot update {}: {error}", target.display()));
        }
        if let Err(error) = std::fs::rename(&staging, &target) {
            // Put the previous skill back rather than leave none.
            let _ = std::fs::rename(&aside, &target);
            let _ = std::fs::remove_dir_all(&staging);
            return fail(format!("cannot update {}: {error}", target.display()));
        }
        let _ = std::fs::remove_dir_all(&aside);
        Ok(target)
    }

    /// Remove a skill from the scope.
    pub fn delete(&self, scope: SkillScope, name: &str) -> Result<(), SkillStoreError> {
        let name = checked_name(name)?;
        let skills_dir = self.skills_dir(scope, false)?;
        let target = skills_dir.join(name);
        existing_directory(&target, scope, name)?;
        let aside = skills_dir.join(format!(".{name}.deleted-{}", unique()));
        std::fs::rename(&target, &aside)
            .or_else(|e| fail(format!("cannot delete {}: {e}", target.display())))?;
        std::fs::remove_dir_all(&aside).or_else(|e| {
            fail(format!(
                "skill \"{name}\" is no longer visible, but {} could not be removed: {e}",
                aside.display()
            ))
        })
    }

    /// Validate a proposal for `scope` and return the directory it would be
    /// written to, without writing anything. Used by the confirmation preview
    /// so a contradiction is refused to the model, never put to the user.
    pub fn validate(
        &self,
        scope: SkillScope,
        draft: &SkillDraft,
    ) -> Result<PathBuf, SkillStoreError> {
        let name = checked_name(&draft.name)?;
        let skills_dir = self.skills_dir(scope, false)?;
        render_skill_md(draft)?;
        Ok(skills_dir.join(name))
    }

    /// The skills directory of `scope`, checked to be where it claims to be.
    fn skills_dir(&self, scope: SkillScope, create: bool) -> Result<PathBuf, SkillStoreError> {
        let (dir, containing_root) = match scope {
            SkillScope::Project => {
                let Some(root) = &self.project_root else {
                    return fail("no project is open, so there is no project skills directory");
                };
                (root.join(".leveler").join("skills"), Some(root.as_path()))
            }
            SkillScope::User => match &self.user_skills_dir {
                Some(dir) => (dir.clone(), None),
                None => {
                    return fail(
                        "no CodeLeveler home is known, so there is no user skills directory",
                    );
                }
            },
            SkillScope::Builtin => {
                return fail("built-in skills are compiled into the binary and cannot be written");
            }
        };
        if create {
            std::fs::create_dir_all(&dir)
                .or_else(|e| fail(format!("cannot create {}: {e}", dir.display())))?;
        }
        if let Some(root) = containing_root {
            let inside = match (dir.canonicalize(), root.canonicalize()) {
                (Ok(dir), Ok(root)) => dir.starts_with(root),
                // Not created yet (update/delete of nothing): nothing to escape.
                (Err(_), _) => true,
                (_, Err(_)) => false,
            };
            if !inside {
                return fail(format!(
                    "{} resolves outside the project; refusing to write skills there",
                    dir.display()
                ));
            }
        }
        Ok(dir)
    }
}

fn checked_name(name: &str) -> Result<&str, SkillStoreError> {
    let name = name.trim();
    name::validate_skill_name(name).map_err(SkillStoreError)?;
    if name::is_builtin_name(name) {
        return fail(format!(
            "\"{name}\" is a built-in skill and cannot be written; save a project or user \
             skill under a different name instead"
        ));
    }
    Ok(name)
}

/// Validate a draft and render its `SKILL.md`. Returns the file's contents.
fn render_skill_md(draft: &SkillDraft) -> Result<String, SkillStoreError> {
    let name = checked_name(&draft.name)?;
    let description = draft.description.trim();
    if description.is_empty() {
        return fail("a skill needs a non-empty description");
    }
    if description.contains(['\n', '\r']) {
        return fail("the description must be a single line (it is the trigger the model reads)");
    }
    if description.chars().count() > MAX_DESCRIPTION_CHARS {
        return fail(format!(
            "the description is longer than {MAX_DESCRIPTION_CHARS} characters"
        ));
    }
    if draft.body.trim().is_empty() {
        return fail("a skill needs a non-empty SKILL.md body");
    }
    if draft.body.len() > MAX_BODY_BYTES {
        return fail(format!("the body is longer than {MAX_BODY_BYTES} bytes"));
    }
    if draft.files.len() > MAX_BUNDLED_FILES {
        return fail(format!(
            "at most {MAX_BUNDLED_FILES} bundled files are allowed"
        ));
    }
    let mut total = 0usize;
    let mut seen: Vec<PathBuf> = Vec::new();
    for file in &draft.files {
        if file.content.len() > MAX_FILE_BYTES {
            return fail(format!(
                "bundled file `{}` is larger than {MAX_FILE_BYTES} bytes",
                file.path
            ));
        }
        total += file.content.len();
        if total > MAX_BUNDLED_BYTES {
            return fail(format!(
                "bundled files total more than {MAX_BUNDLED_BYTES} bytes"
            ));
        }
        let relative = validate_bundled_path(&file.path)?;
        if seen.contains(&relative) {
            return fail(format!("bundled path `{}` is listed twice", file.path));
        }
        seen.push(relative);
    }
    let rendered = frontmatter::render(name, description, &draft.body);
    // The rendered form must parse back to the identity and description the
    // caller proposed, or the registry would see something else.
    let parsed = frontmatter::parse(&rendered)
        .map_err(|error| SkillStoreError(format!("the rendered SKILL.md is invalid: {error}")))?;
    if parsed.frontmatter.name() != Some(name)
        || parsed.frontmatter.description().as_deref() != Some(description)
    {
        return fail("the rendered SKILL.md does not round-trip");
    }
    Ok(rendered)
}

/// A bundled path must be relative and stay inside the skill directory, and
/// must not take the package's own manifest name.
fn validate_bundled_path(path: &str) -> Result<PathBuf, SkillStoreError> {
    let normalized = path.replace('\\', "/");
    if normalized.trim().is_empty() {
        return fail("a bundled file path is empty");
    }
    let candidate = PathBuf::from(&normalized);
    if candidate.is_absolute() || candidate.has_root() {
        return fail(format!("bundled path `{path}` must be relative"));
    }
    let mut out = PathBuf::new();
    for component in candidate.components() {
        match component {
            Component::Normal(part) => {
                if out.as_os_str().is_empty() && part.eq_ignore_ascii_case("SKILL.md") {
                    return fail(format!("bundled path `{path}` may not be SKILL.md"));
                }
                out.push(part);
            }
            Component::CurDir => {}
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => {
                return fail(format!(
                    "bundled path `{path}` must stay inside the skill directory"
                ));
            }
        }
    }
    if out.as_os_str().is_empty() {
        return fail(format!("bundled path `{path}` is empty"));
    }
    Ok(out)
}

fn existing_directory(target: &Path, scope: SkillScope, name: &str) -> Result<(), SkillStoreError> {
    match std::fs::symlink_metadata(target) {
        Ok(meta) if meta.is_dir() => Ok(()),
        Ok(_) => fail(format!(
            "{} is not a plain directory; refusing to modify it",
            target.display()
        )),
        Err(_) => fail(format!(
            "{} skill \"{name}\" does not exist",
            scope.as_str()
        )),
    }
}

/// Write `SKILL.md` and every bundled file into a fresh hidden directory beside
/// the target.
fn stage(
    skills_dir: &Path,
    name: &str,
    skill_md: &str,
    files: &[SkillFile],
) -> Result<PathBuf, SkillStoreError> {
    let staging = skills_dir.join(format!(".{name}.staging-{}", unique()));
    let write = || -> std::io::Result<()> {
        std::fs::create_dir(&staging)?;
        write_file(&staging.join("SKILL.md"), skill_md.as_bytes())?;
        for file in files {
            let relative = validate_bundled_path(&file.path)
                .map_err(|error| std::io::Error::other(error.to_string()))?;
            let destination = staging.join(&relative);
            if let Some(parent) = destination.parent() {
                std::fs::create_dir_all(parent)?;
            }
            write_file(&destination, file.content.as_bytes())?;
        }
        Ok(())
    };
    if let Err(error) = write() {
        let _ = std::fs::remove_dir_all(&staging);
        return fail(format!(
            "cannot write the skill under {}: {error}",
            skills_dir.display()
        ));
    }
    Ok(staging)
}

fn write_file(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let mut file = std::fs::File::create(path)?;
    file.write_all(bytes)?;
    file.sync_all()
}

fn unique() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or_default();
    format!(
        "{}-{nanos}-{}",
        std::process::id(),
        COUNTER.fetch_add(1, Ordering::Relaxed)
    )
}
