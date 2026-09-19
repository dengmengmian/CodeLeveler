//! The resolved skill read model: every root is scanned once, names are
//! resolved by precedence, and every consumer (the context index, `$mention`,
//! `load_skill`, `Agent.skills`, the CLI and the TUI) reads this one answer.
//!
//! Identity is fixed here: the **directory name is the canonical name**. A
//! frontmatter `name` that disagrees is an invalid package, not a second
//! identity — so the index and loading can never disagree about what a skill
//! is called.
//!
//! Precedence is locality first: project scope beats user scope, and within a
//! scope the roots are checked in the order [`SkillRoots`] lists them. An
//! invalid higher-precedence definition still wins its name (it is visible as
//! invalid) rather than silently falling through to a lower-precedence skill
//! of the same name.

use std::ffi::OsString;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::builtin;
use crate::frontmatter;
use crate::name;
use crate::render;

/// Where a skill lives, in locality order. Project is the nearest scope, then
/// the user's own roots, then what ships with the binary.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SkillScope {
    Project,
    User,
    Builtin,
}

impl SkillScope {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Project => "project",
            Self::User => "user",
            Self::Builtin => "builtin",
        }
    }
}

impl std::fmt::Display for SkillScope {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Which ecosystem's package format a root holds. This is a provenance label,
/// not a precedence rule; precedence is [`SkillScope`] plus root order.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SkillSource {
    /// CodeLeveler's own store: `<repo>/.leveler/skills`, `~/.leveler/skills`.
    Native,
    /// Codex-compatible: `~/.codex/skills`.
    Codex,
    /// The Agent Skills standard: `.agents/skills`.
    AgentSkills,
    /// Claude Code-compatible: `.claude/skills`.
    Claude,
    /// Compiled into the binary.
    Builtin,
}

impl SkillSource {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Native => "native",
            Self::Codex => "codex",
            Self::AgentSkills => "agent-skills",
            Self::Claude => "claude",
            Self::Builtin => "builtin",
        }
    }

    /// The human label a client shows, e.g. `Codex compatible`.
    pub fn label(self) -> &'static str {
        match self {
            Self::Native => "CodeLeveler",
            Self::Codex => "Codex compatible",
            Self::AgentSkills => "Agent Skills",
            Self::Claude => "Claude compatible",
            Self::Builtin => "Built-in",
        }
    }
}

impl std::fmt::Display for SkillSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// One directory the registry scans. Roots are listed highest precedence first.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkillRoot {
    pub scope: SkillScope,
    pub source: SkillSource,
    pub dir: PathBuf,
    /// For project scope: the directory must canonicalize inside this root, so
    /// a repository cannot point `.leveler/skills` anywhere on the machine.
    pub containing_root: Option<PathBuf>,
}

/// Every root a registry reads. The order *is* the precedence order.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SkillRoots {
    roots: Vec<SkillRoot>,
}

impl SkillRoots {
    /// No roots at all (tests, or a caller that will push its own).
    pub fn empty() -> Self {
        Self { roots: Vec::new() }
    }

    /// The roots visible from a project, resolved from the live environment.
    pub fn for_project(project_root: &Path) -> Self {
        Self::for_project_in(project_root, &|key| leveler_core::environment().var_os(key))
    }

    /// [`Self::for_project`] with an injected environment lookup, for tests and
    /// for surfaces that hold their own [`leveler_core::EnvSnapshot`].
    pub fn for_project_in<F>(project_root: &Path, var: &F) -> Self
    where
        F: Fn(&str) -> Option<OsString>,
    {
        let mut roots = Vec::new();
        for &(source, relative) in PROJECT_ROOTS {
            roots.push(SkillRoot {
                scope: SkillScope::Project,
                source,
                dir: project_root.join(relative),
                containing_root: Some(project_root.to_path_buf()),
            });
        }
        if let Some(home) = leveler_core::leveler_home_dir_from(var) {
            roots.push(SkillRoot {
                scope: SkillScope::User,
                source: SkillSource::Native,
                dir: leveler_core::LevelerHome::from_root(home).skills_dir(),
                containing_root: None,
            });
        }
        if let Some(home) = user_home(var) {
            for &(source, relative) in USER_ROOTS {
                roots.push(SkillRoot {
                    scope: SkillScope::User,
                    source,
                    dir: home.join(relative),
                    containing_root: None,
                });
            }
        }
        Self { roots }
    }

    pub fn push(&mut self, root: SkillRoot) {
        self.roots.push(root);
    }

    pub fn roots(&self) -> &[SkillRoot] {
        &self.roots
    }

    pub fn is_empty(&self) -> bool {
        self.roots.is_empty()
    }
}

/// Project-scope roots, highest precedence first: CodeLeveler's own store, then
/// the compatible ecosystems the machine may already have installed.
const PROJECT_ROOTS: &[(SkillSource, &str)] = &[
    (SkillSource::Native, ".leveler/skills"),
    (SkillSource::Codex, ".codex/skills"),
    (SkillSource::AgentSkills, ".agents/skills"),
    (SkillSource::Claude, ".claude/skills"),
];

/// User-scope roots, in deterministic same-scope precedence order. The native
/// store is first because it is the one CodeLeveler itself manages; the
/// compatible ecosystems follow in a fixed order so a clash is explainable.
const USER_ROOTS: &[(SkillSource, &str)] = &[
    (SkillSource::Codex, ".codex/skills"),
    (SkillSource::AgentSkills, ".agents/skills"),
    (SkillSource::Claude, ".claude/skills"),
];

fn user_home<F>(var: &F) -> Option<PathBuf>
where
    F: Fn(&str) -> Option<OsString>,
{
    var("HOME")
        .filter(|v| !v.is_empty())
        .or_else(|| var("USERPROFILE").filter(|v| !v.is_empty()))
        .map(PathBuf::from)
}

/// The lightweight index entry loaded into context.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SkillSummary {
    pub name: String,
    pub description: String,
    pub scope: SkillScope,
    pub source: SkillSource,
}

/// A skill's full content, loaded on demand or turn-injected.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkillDetail {
    pub name: String,
    pub description: String,
    /// The Markdown body after the frontmatter.
    pub body: String,
    pub scope: SkillScope,
    pub source: SkillSource,
    /// Absolute skill package directory (contains `SKILL.md`). A marker
    /// (`(built-in)`) for a compiled skill, which has no directory.
    pub dir: PathBuf,
    /// Paths relative to [`Self::dir`] under `scripts/` (and nested).
    pub scripts: Vec<String>,
    /// Paths relative to [`Self::dir`] under `references/` (and nested).
    pub references: Vec<String>,
    /// Other bundled files (not SKILL.md, not under scripts/ or references/).
    pub other_files: Vec<String>,
}

/// Result of resolving `$name` mentions in a user message.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct SkillMentionResolution {
    /// Skills successfully loaded for this turn (order of first mention).
    pub loaded: Vec<SkillDetail>,
    /// `$name` tokens that matched no skill at all.
    pub unknown: Vec<String>,
    /// `$name` tokens that matched a skill that exists but is unusable, with
    /// the reason. Kept apart from `unknown` so a broken package is diagnosable
    /// instead of looking like a typo.
    pub invalid: Vec<(String, String)>,
}

impl SkillMentionResolution {
    pub fn is_empty(&self) -> bool {
        self.loaded.is_empty() && self.unknown.is_empty() && self.invalid.is_empty()
    }
}

/// Why a name did not resolve to a usable skill.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SkillLookupError {
    NotFound {
        name: String,
        available: Vec<String>,
    },
    Invalid {
        name: String,
        scope: SkillScope,
        source: SkillSource,
        location: Option<PathBuf>,
        error: String,
    },
    Read {
        name: String,
        error: String,
    },
}

impl std::fmt::Display for SkillLookupError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotFound { name, available } => {
                if available.is_empty() {
                    write!(f, "no skill named `{name}`")
                } else {
                    write!(
                        f,
                        "no skill named `{name}` (available: {})",
                        available.join(", ")
                    )
                }
            }
            Self::Invalid {
                name,
                scope,
                source,
                location,
                error,
            } => {
                write!(f, "skill `{name}` ({source} · {scope}) is invalid: {error}")?;
                if let Some(location) = location {
                    write!(f, " [{}]", location.display())?;
                }
                Ok(())
            }
            Self::Read { name, error } => write!(f, "skill `{name}` could not be read: {error}"),
        }
    }
}

impl std::error::Error for SkillLookupError {}

/// A lower-precedence definition hidden by the active one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShadowedSkill {
    pub scope: SkillScope,
    pub source: SkillSource,
    pub location: Option<PathBuf>,
    /// The reason that definition is unusable, when it is.
    pub error: Option<String>,
}

/// A registry entry's structural state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SkillState {
    Available,
    /// The definition exists but cannot be used; the reason is actionable.
    Invalid(String),
}

/// One resolvable skill name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkillEntry {
    pub name: String,
    pub scope: SkillScope,
    pub source: SkillSource,
    /// The package directory; `None` for built-ins.
    pub location: Option<PathBuf>,
    /// Present only for an available entry.
    pub description: Option<String>,
    pub state: SkillState,
    pub shadowed: Vec<ShadowedSkill>,
}

impl SkillEntry {
    pub fn is_available(&self) -> bool {
        matches!(self.state, SkillState::Available)
    }

    pub fn invalid_reason(&self) -> Option<&str> {
        match &self.state {
            SkillState::Available => None,
            SkillState::Invalid(reason) => Some(reason),
        }
    }
}

/// Something under a skills directory that is not a loadable skill at all (an
/// unusable directory name, a root that escapes the project).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkillProblem {
    pub scope: SkillScope,
    pub source: SkillSource,
    pub location: PathBuf,
    pub error: String,
}

struct Candidate {
    rank: usize,
    name: String,
    scope: SkillScope,
    source: SkillSource,
    location: Option<PathBuf>,
    description: Option<String>,
    error: Option<String>,
}

/// Every skill visible from one project, resolved by precedence.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SkillRegistry {
    entries: Vec<SkillEntry>,
    problems: Vec<SkillProblem>,
}

impl SkillRegistry {
    /// Scan every root once. Never fails as a whole: a broken package becomes an
    /// invalid entry or a problem, not a failed start.
    pub fn load(roots: &SkillRoots) -> Self {
        let mut candidates: Vec<Candidate> = Vec::new();
        let mut problems: Vec<SkillProblem> = Vec::new();
        for (rank, root) in roots.roots.iter().enumerate() {
            read_root(root, rank, &mut candidates, &mut problems);
        }
        let builtin_rank = roots.roots.len();
        for skill in builtin::skills() {
            candidates.push(Candidate {
                rank: builtin_rank,
                name: skill.name.to_string(),
                scope: SkillScope::Builtin,
                source: SkillSource::Builtin,
                location: None,
                description: Some(skill.description.to_string()),
                error: None,
            });
        }
        // Stable sort: name, then precedence rank (the root order).
        candidates.sort_by(|a, b| a.name.cmp(&b.name).then(a.rank.cmp(&b.rank)));
        let mut entries: Vec<SkillEntry> = Vec::new();
        for candidate in candidates {
            match entries.last_mut() {
                Some(active) if active.name == candidate.name => {
                    active.shadowed.push(ShadowedSkill {
                        scope: candidate.scope,
                        source: candidate.source,
                        location: candidate.location,
                        error: candidate.error,
                    });
                }
                _ => entries.push(SkillEntry {
                    name: candidate.name,
                    scope: candidate.scope,
                    source: candidate.source,
                    location: candidate.location,
                    description: candidate.description,
                    state: match candidate.error {
                        Some(error) => SkillState::Invalid(error),
                        None => SkillState::Available,
                    },
                    shadowed: Vec::new(),
                }),
            }
        }
        Self { entries, problems }
    }

    pub fn entries(&self) -> &[SkillEntry] {
        &self.entries
    }

    pub fn problems(&self) -> &[SkillProblem] {
        &self.problems
    }

    pub fn get(&self, name: &str) -> Option<&SkillEntry> {
        self.entries.iter().find(|e| e.name == name)
    }

    /// The index: names + descriptions of every usable skill.
    pub fn summaries(&self) -> Vec<SkillSummary> {
        self.entries
            .iter()
            .filter_map(|entry| {
                let description = entry.description.clone()?;
                entry.is_available().then_some(SkillSummary {
                    name: entry.name.clone(),
                    description,
                    scope: entry.scope,
                    source: entry.source,
                })
            })
            .collect()
    }

    /// Usable names, sorted (the entries are already name-sorted).
    pub fn available_names(&self) -> Vec<String> {
        self.entries
            .iter()
            .filter(|e| e.is_available())
            .map(|e| e.name.clone())
            .collect()
    }

    /// Resolve a name to a usable entry. Never falls back: an invalid active
    /// definition is refused with its reason rather than skipped.
    pub fn resolve(&self, name: &str) -> Result<&SkillEntry, SkillLookupError> {
        let requested = name.trim();
        let Some(entry) = self.get(requested) else {
            return Err(SkillLookupError::NotFound {
                name: requested.to_string(),
                available: self.available_names(),
            });
        };
        match &entry.state {
            SkillState::Available => Ok(entry),
            SkillState::Invalid(error) => Err(SkillLookupError::Invalid {
                name: entry.name.clone(),
                scope: entry.scope,
                source: entry.source,
                location: entry.location.clone(),
                error: error.clone(),
            }),
        }
    }

    /// Read the full body and bundled files of the resolved winner.
    pub fn load_skill(&self, name: &str) -> Result<SkillDetail, SkillLookupError> {
        let entry = self.resolve(name)?;
        if entry.source == SkillSource::Builtin {
            return builtin::detail(&entry.name).ok_or_else(|| SkillLookupError::Read {
                name: entry.name.clone(),
                error: "the built-in skill is not compiled into this binary".to_string(),
            });
        }
        let dir = entry
            .location
            .clone()
            .ok_or_else(|| SkillLookupError::Read {
                name: entry.name.clone(),
                error: "the skill has no directory".to_string(),
            })?;
        let content = std::fs::read_to_string(dir.join("SKILL.md")).map_err(|error| {
            SkillLookupError::Read {
                name: entry.name.clone(),
                error: error.to_string(),
            }
        })?;
        let parsed = frontmatter::parse(&content).map_err(|error| SkillLookupError::Read {
            name: entry.name.clone(),
            error,
        })?;
        let (scripts, references, other_files) = classify_bundled_files(&dir);
        Ok(SkillDetail {
            name: entry.name.clone(),
            description: entry.description.clone().unwrap_or_default(),
            body: parsed.body,
            scope: entry.scope,
            source: entry.source,
            dir,
            scripts,
            references,
            other_files,
        })
    }

    /// Resolve `$name` mentions in `text` against this registry.
    pub fn resolve_mentions(&self, text: &str) -> SkillMentionResolution {
        let mut loaded = Vec::new();
        let mut unknown = Vec::new();
        let mut invalid = Vec::new();
        for name in render::parse_skill_mentions(text) {
            match self.load_skill(&name) {
                Ok(detail) => loaded.push(detail),
                Err(SkillLookupError::NotFound { name, .. }) => unknown.push(name),
                Err(SkillLookupError::Invalid { name, error, .. })
                | Err(SkillLookupError::Read { name, error }) => invalid.push((name, error)),
            }
        }
        SkillMentionResolution {
            loaded,
            unknown,
            invalid,
        }
    }
}

fn read_root(
    root: &SkillRoot,
    rank: usize,
    candidates: &mut Vec<Candidate>,
    problems: &mut Vec<SkillProblem>,
) {
    let Ok(listing) = std::fs::read_dir(&root.dir) else {
        return;
    };
    // A project's skills directory must be inside the project: a symlinked
    // root would let a repository point the registry anywhere.
    if let Some(containing) = &root.containing_root {
        let inside = match (root.dir.canonicalize(), containing.canonicalize()) {
            (Ok(dir), Ok(containing)) => dir.starts_with(containing),
            // Not created yet: nothing to load, nothing to escape.
            (Err(_), _) => true,
            (_, Err(_)) => false,
        };
        if !inside {
            problems.push(SkillProblem {
                scope: root.scope,
                source: root.source,
                location: root.dir.clone(),
                error: "the skills directory resolves outside the project; no skills were \
                        loaded from it"
                    .into(),
            });
            return;
        }
    }
    for entry in listing.flatten() {
        let path = entry.path();
        let Some(dir_name) = entry.file_name().to_str().map(str::to_string) else {
            problems.push(SkillProblem {
                scope: root.scope,
                source: root.source,
                location: path,
                error: "the directory name is not valid UTF-8".into(),
            });
            continue;
        };
        // Hidden entries are staging directories and editor files.
        if dir_name.starts_with('.') {
            continue;
        }
        let Ok(file_type) = entry.file_type() else {
            continue;
        };
        if !file_type.is_dir() && !file_type.is_symlink() {
            continue;
        }
        if let Err(error) = name::validate_read_name(&dir_name) {
            problems.push(SkillProblem {
                scope: root.scope,
                source: root.source,
                location: path,
                error,
            });
            continue;
        }
        // A project skill directory that resolves outside the project is not
        // this project's skill, whatever it claims.
        if let Some(containing) = &root.containing_root
            && let (Ok(real), Ok(containing)) = (path.canonicalize(), containing.canonicalize())
            && !real.starts_with(&containing)
        {
            problems.push(SkillProblem {
                scope: root.scope,
                source: root.source,
                location: path,
                error: "the skill directory resolves outside the project; it was not loaded".into(),
            });
            continue;
        }
        candidates.push(read_candidate(root, rank, &path, &dir_name));
    }
}

fn read_candidate(root: &SkillRoot, rank: usize, dir: &Path, name: &str) -> Candidate {
    let invalid = |error: String| Candidate {
        rank,
        name: name.to_string(),
        scope: root.scope,
        source: root.source,
        location: Some(dir.to_path_buf()),
        description: None,
        error: Some(error),
    };
    let skill_md = dir.join("SKILL.md");
    let Ok(meta) = std::fs::symlink_metadata(&skill_md) else {
        return invalid("missing SKILL.md".to_string());
    };
    // A symlinked SKILL.md may point at the package's real file, but not out of
    // the package.
    if meta.file_type().is_symlink()
        && let (Ok(target), Ok(base)) = (skill_md.canonicalize(), dir.canonicalize())
        && !target.starts_with(&base)
    {
        return invalid(
            "SKILL.md is a symlink that points outside the skill directory".to_string(),
        );
    }
    let Ok(bytes) = std::fs::read(&skill_md) else {
        return invalid("SKILL.md could not be read".to_string());
    };
    let Ok(content) = String::from_utf8(bytes) else {
        return invalid("SKILL.md is not valid UTF-8".to_string());
    };
    let parsed = match frontmatter::parse(&content) {
        Ok(parsed) => parsed,
        Err(error) => return invalid(error),
    };
    if let Some(declared) = parsed.frontmatter.name()
        && declared != name
    {
        return invalid(format!(
            "name mismatch: directory `{name}`, manifest `{declared}`"
        ));
    }
    let Some(description) = parsed.frontmatter.description() else {
        return invalid("SKILL.md needs a non-empty `description`".to_string());
    };
    Candidate {
        rank,
        name: name.to_string(),
        scope: root.scope,
        source: root.source,
        location: Some(dir.to_path_buf()),
        description: Some(description),
        error: None,
    }
}

/// Walk a skill package directory and classify relative paths. Symlinked
/// directories are not descended into, so a bundled path cannot escape the
/// package.
fn classify_bundled_files(dir: &Path) -> (Vec<String>, Vec<String>, Vec<String>) {
    let mut scripts = Vec::new();
    let mut references = Vec::new();
    let mut other_files = Vec::new();
    walk_bundled(dir, &mut scripts, &mut references, &mut other_files);
    scripts.sort();
    references.sort();
    other_files.sort();
    (scripts, references, other_files)
}

fn walk_bundled(
    root: &Path,
    scripts: &mut Vec<String>,
    references: &mut Vec<String>,
    other_files: &mut Vec<String>,
) {
    walk_dir(root, root, scripts, references, other_files);
}

fn walk_dir(
    root: &Path,
    current: &Path,
    scripts: &mut Vec<String>,
    references: &mut Vec<String>,
    other_files: &mut Vec<String>,
) {
    let Ok(entries) = std::fs::read_dir(current) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let name = entry.file_name().to_string_lossy().into_owned();
        if name == "SKILL.md" {
            continue;
        }
        let Ok(file_type) = entry.file_type() else {
            continue;
        };
        if file_type.is_symlink() {
            // Do not follow symlinks out of the package (or at all, for
            // bundled content): list the link's own name as an ordinary file.
            if let Ok(rel) = path.strip_prefix(root) {
                push_classified(
                    &rel.to_string_lossy().replace('\\', "/"),
                    scripts,
                    references,
                    other_files,
                );
            }
            continue;
        }
        if file_type.is_dir() {
            walk_dir(root, &path, scripts, references, other_files);
            continue;
        }
        let Ok(rel) = path.strip_prefix(root) else {
            continue;
        };
        push_classified(
            &rel.to_string_lossy().replace('\\', "/"),
            scripts,
            references,
            other_files,
        );
    }
}

fn push_classified(
    relative: &str,
    scripts: &mut Vec<String>,
    references: &mut Vec<String>,
    other_files: &mut Vec<String>,
) {
    if relative == "scripts" || relative.starts_with("scripts/") {
        scripts.push(relative.to_string());
    } else if relative == "references" || relative.starts_with("references/") {
        references.push(relative.to_string());
    } else {
        other_files.push(relative.to_string());
    }
}

/// Discover skills from every visible root, resolved by precedence.
pub fn discover(root: &Path) -> Vec<SkillSummary> {
    SkillRegistry::load(&SkillRoots::for_project(root)).summaries()
}

/// The full resolved registry for a project (the read model the CLI and TUI
/// render, and the one `discover`/`load` are derived from).
pub fn describe(root: &Path) -> SkillRegistry {
    SkillRegistry::load(&SkillRoots::for_project(root))
}

/// Load a skill's full body and structured bundled files by name.
pub fn load(root: &Path, name: &str) -> Option<SkillDetail> {
    describe(root).load_skill(name).ok()
}

/// Resolve `$name` mentions against every visible root.
pub fn resolve_mentions(root: &Path, text: &str) -> SkillMentionResolution {
    describe(root).resolve_mentions(text)
}
