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

use leveler_definition_trust::{
    DefinitionIdentity, DefinitionKind, DefinitionTrust, DefinitionTrustView, TrustAuditEntry,
};
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

/// Longest reference text a single `load_skill` may hand to the model.
///
/// References are content-bound, so they are returned as verified text rather
/// than as paths the model must re-read. The bound leaves room under the
/// registry's 48 KiB tool-result cap for the skill body and its framing; a
/// larger package fails closed with an actionable error instead of returning
/// unverified bytes or a silently truncated procedure.
const MAX_INLINE_REFERENCE_BYTES: usize = 32 * 1024;

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

/// A present-but-untrusted skill, as the index may name it.
///
/// It deliberately carries no description: unapproved package metadata must
/// not be presented as usable instruction. The name and provenance are what
/// the model needs to request the skill, which triggers the human decision.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UntrustedSkillSummary {
    pub name: String,
    pub scope: SkillScope,
    pub source: SkillSource,
}

/// One verified reference file's text. The bytes were read and hashed as part of
/// the package approval in the same pass that produced this value, so the model
/// never receives text that a later edit could have swapped.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkillReferenceContent {
    /// Package-relative path (`references/guide.md`).
    pub relative: String,
    pub text: String,
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
    /// Verified text of every reference file, in the same order as
    /// [`Self::references`]. These are the bytes the trust approval bound.
    pub reference_contents: Vec<SkillReferenceContent>,
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
    /// The package exists but its exact bytes are not approved.
    ///
    /// The bulky detail lives behind a box: this refusal crosses every
    /// resolution call, and the common successful path must not pay for it in
    /// the error type's stack width.
    Untrusted {
        name: String,
        scope: SkillScope,
        source: SkillSource,
        state: String,
        package: Option<Box<UntrustedSkill>>,
    },
}

impl SkillLookupError {
    /// The package an authorization request must name, when this is an
    /// untrusted-content refusal.
    pub fn untrusted_package(&self) -> Option<UntrustedSkill> {
        match self {
            Self::Untrusted { package, .. } => package.as_deref().cloned(),
            _ => None,
        }
    }
}

/// A package that exists but was refused by the trust gate.
///
/// It is kept apart from the resolved entries: an unapproved package must not
/// shadow a trusted lower-precedence skill, and its description must never be
/// presented as usable metadata. The name, scope, source and location are the
/// facts an authorization request needs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UntrustedSkill {
    pub name: String,
    pub scope: SkillScope,
    pub source: SkillSource,
    pub location: PathBuf,
    pub identity: Option<DefinitionIdentity>,
    pub digest: String,
    /// The full decision (state + any approved digest).
    pub decision: DefinitionTrust,
}

impl UntrustedSkill {
    pub fn state(&self) -> &'static str {
        self.decision.audit_state()
    }

    /// Flatten to the audit record the registry has always exposed.
    pub fn audit(&self) -> TrustAuditEntry {
        TrustAuditEntry {
            id: self
                .identity
                .as_ref()
                .map(|identity| identity.id.clone())
                .unwrap_or_else(|| format!("skill:{}", self.name)),
            display: self
                .identity
                .as_ref()
                .map(|identity| identity.display.clone())
                .unwrap_or_else(|| self.name.clone()),
            scope: self.scope.as_str().to_string(),
            kind: "skill".to_string(),
            state: self.state().to_string(),
            approved_digest: self.decision.approved_digest().map(str::to_string),
            store_status: None,
            session_source: None,
        }
    }
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
            Self::Untrusted {
                name,
                scope,
                source,
                state,
                ..
            } => write!(
                f,
                "skill `{name}` ({source} · {scope}) is present but not trusted ({state}); \
                 the user can review and approve it with /trust"
            ),
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
#[derive(Debug, Clone)]
pub struct SkillRegistry {
    entries: Vec<SkillEntry>,
    problems: Vec<SkillProblem>,
    /// Packages present but refused by the trust gate. They are not entries:
    /// an unapproved package never shadows a trusted one.
    untrusted: Vec<TrustAuditEntry>,
    /// The same refusals with the fields an authorization request needs.
    /// Ordered by root precedence, so the first match for a name is the one
    /// the loader would have used.
    untrusted_skills: Vec<UntrustedSkill>,
    /// The snapshot `load_skill` re-checks before returning a body.
    trust: DefinitionTrustView,
}

impl Default for SkillRegistry {
    fn default() -> Self {
        Self {
            entries: Vec::new(),
            problems: Vec::new(),
            untrusted: Vec::new(),
            untrusted_skills: Vec::new(),
            trust: DefinitionTrustView::deny_all(),
        }
    }
}

impl PartialEq for SkillRegistry {
    fn eq(&self, other: &Self) -> bool {
        self.entries == other.entries
            && self.problems == other.problems
            && self.untrusted == other.untrusted
            && self.untrusted_skills == other.untrusted_skills
    }
}

impl Eq for SkillRegistry {}

impl SkillRegistry {
    /// Scan every root once. Never fails as a whole: a broken package becomes an
    /// invalid entry or a problem, not a failed start. A package the trust gate
    /// refuses is reported beside the entries and cannot shadow a trusted one.
    pub fn load(roots: &SkillRoots, trust: &DefinitionTrustView) -> Self {
        let mut candidates: Vec<Candidate> = Vec::new();
        let mut problems: Vec<SkillProblem> = Vec::new();
        let mut untrusted: Vec<TrustAuditEntry> = Vec::new();
        let mut untrusted_skills: Vec<UntrustedSkill> = Vec::new();
        for (rank, root) in roots.roots.iter().enumerate() {
            read_root(
                root,
                rank,
                trust,
                &mut candidates,
                &mut problems,
                &mut untrusted,
                &mut untrusted_skills,
            );
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
        Self {
            entries,
            problems,
            untrusted,
            untrusted_skills,
            trust: trust.clone(),
        }
    }

    /// Packages present but not admitted by the trust gate.
    pub fn untrusted(&self) -> &[TrustAuditEntry] {
        &self.untrusted
    }

    /// The refused packages with the identity and digest an authorization
    /// request needs, in precedence order.
    pub fn untrusted_skills(&self) -> &[UntrustedSkill] {
        &self.untrusted_skills
    }

    /// The highest-precedence refused package for `name`.
    ///
    /// This is only consulted when no trusted entry owns the name, so an
    /// unapproved package can never shadow a usable one.
    pub fn untrusted_skill(&self, name: &str) -> Option<&UntrustedSkill> {
        self.untrusted_skills
            .iter()
            .find(|skill| skill.name == name)
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

    /// Refused names that do not collide with a usable entry.
    ///
    /// The index names these so the model can request one, but only when no
    /// trusted entry owns the name: an unapproved package never shadows a
    /// usable skill, in the index or in resolution.
    pub fn untrusted_summaries(&self) -> Vec<UntrustedSkillSummary> {
        let mut seen = std::collections::BTreeSet::new();
        let mut out = Vec::new();
        for skill in &self.untrusted_skills {
            if self.get(&skill.name).is_some() || !seen.insert(skill.name.clone()) {
                continue;
            }
            out.push(UntrustedSkillSummary {
                name: skill.name.clone(),
                scope: skill.scope,
                source: skill.source,
            });
        }
        out
    }

    /// Resolve a name to a usable entry. Never falls back: an invalid active
    /// definition is refused with its reason rather than skipped.
    pub fn resolve(&self, name: &str) -> Result<&SkillEntry, SkillLookupError> {
        let requested = name.trim();
        let Some(entry) = self.get(requested) else {
            // A package that exists but was refused is not a typo. Name it so
            // the caller can ask the user instead of the model guessing.
            if let Some(untrusted) = self.untrusted_skill(requested) {
                return Err(SkillLookupError::Untrusted {
                    name: untrusted.name.clone(),
                    scope: untrusted.scope,
                    source: untrusted.source,
                    state: untrusted.state().to_string(),
                    package: Some(Box::new(untrusted.clone())),
                });
            }
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
        // Read SKILL.md and every reference once. The digest covers exactly
        // these bytes, and the text handed to the model comes from the same
        // values, so a hash/read race cannot show unapproved instructions.
        let package = leveler_definition_trust::read_skill_package(&dir.join("SKILL.md")).map_err(
            |error| SkillLookupError::Read {
                name: entry.name.clone(),
                error,
            },
        )?;
        // Re-check the bytes actually about to be returned. A registry built
        // before an edit must not hand out content the approval never covered.
        if let Some(identity) = identity_for(entry.scope, entry.source, &entry.name) {
            let digest = DefinitionIdentity::skill_package_digest(&package);
            let decision = self.trust.check(&identity, &digest);
            if !decision.admits() {
                let package = UntrustedSkill {
                    name: entry.name.clone(),
                    scope: entry.scope,
                    source: entry.source,
                    location: dir,
                    identity: Some(identity),
                    digest,
                    decision,
                };
                return Err(SkillLookupError::Untrusted {
                    name: package.name.clone(),
                    scope: package.scope,
                    source: package.source,
                    state: package.state().to_string(),
                    package: Some(Box::new(package)),
                });
            }
        }
        let mut skill_body = None;
        let mut references = Vec::new();
        let mut reference_contents = Vec::new();
        let mut inline_bytes = 0usize;
        for part in package {
            if part.relative == "SKILL.md" {
                skill_body = Some(part.bytes);
                continue;
            }
            inline_bytes = inline_bytes.saturating_add(part.bytes.len());
            if inline_bytes > MAX_INLINE_REFERENCE_BYTES {
                return Err(SkillLookupError::Read {
                    name: entry.name.clone(),
                    error: format!(
                        "the skill's references exceed the {MAX_INLINE_REFERENCE_BYTES}-byte \
                         verified-content budget; split the procedure or move non-instruction \
                         assets out of `references/`"
                    ),
                });
            }
            let text = String::from_utf8(part.bytes).map_err(|_| SkillLookupError::Read {
                name: entry.name.clone(),
                error: format!("{} is not valid UTF-8", part.relative),
            })?;
            references.push(part.relative.clone());
            reference_contents.push(SkillReferenceContent {
                relative: part.relative,
                text,
            });
        }
        let bytes = skill_body.ok_or_else(|| SkillLookupError::Read {
            name: entry.name.clone(),
            error: "SKILL.md is missing from the approved package".to_string(),
        })?;
        let content = String::from_utf8(bytes).map_err(|_| SkillLookupError::Read {
            name: entry.name.clone(),
            error: "SKILL.md is not valid UTF-8".to_string(),
        })?;
        let parsed = frontmatter::parse(&content).map_err(|error| SkillLookupError::Read {
            name: entry.name.clone(),
            error,
        })?;
        let (scripts, _, other_files) = classify_bundled_files(&dir);
        Ok(SkillDetail {
            name: entry.name.clone(),
            description: entry.description.clone().unwrap_or_default(),
            body: parsed.body,
            scope: entry.scope,
            source: entry.source,
            dir,
            scripts,
            references,
            reference_contents,
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
                Err(SkillLookupError::Untrusted { name, state, .. }) => invalid.push((
                    name.clone(),
                    format!(
                        "not trusted ({state}); call `load_skill(\"{name}\")` to ask the user to \
                         admit it, or the user can review it with /trust"
                    ),
                )),
            }
        }
        SkillMentionResolution {
            loaded,
            unknown,
            invalid,
        }
    }
}

fn identity_for(scope: SkillScope, source: SkillSource, name: &str) -> Option<DefinitionIdentity> {
    match scope {
        SkillScope::Builtin => None,
        SkillScope::Project => {
            let root = match source {
                SkillSource::Native => ".leveler/skills",
                SkillSource::Codex => ".codex/skills",
                SkillSource::AgentSkills => ".agents/skills",
                SkillSource::Claude => ".claude/skills",
                SkillSource::Builtin => return None,
            };
            DefinitionIdentity::project(DefinitionKind::Skill, &format!("{root}/{name}"))
        }
        SkillScope::User => {
            let label = match source {
                SkillSource::Native => "leveler",
                SkillSource::Codex => "codex",
                SkillSource::AgentSkills => "agents",
                SkillSource::Claude => "claude",
                SkillSource::Builtin => return None,
            };
            DefinitionIdentity::user_skill(label, name)
        }
    }
}

fn read_root(
    root: &SkillRoot,
    rank: usize,
    trust: &DefinitionTrustView,
    candidates: &mut Vec<Candidate>,
    problems: &mut Vec<SkillProblem>,
    untrusted: &mut Vec<TrustAuditEntry>,
    untrusted_skills: &mut Vec<UntrustedSkill>,
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
        match read_candidate(root, rank, &path, &dir_name, trust) {
            CandidateRead::Candidate(candidate) => candidates.push(candidate),
            CandidateRead::Untrusted(untrusted_skill) => {
                // Do not enter the name: an unapproved package must not shadow
                // a trusted lower-precedence skill.
                problems.push(SkillProblem {
                    scope: root.scope,
                    source: root.source,
                    location: path,
                    error: format!(
                        "the skill is not trusted ({}); the user can approve it with /trust",
                        untrusted_skill.state()
                    ),
                });
                untrusted.push(untrusted_skill.audit());
                untrusted_skills.push(untrusted_skill);
            }
        }
    }
}

enum CandidateRead {
    Candidate(Candidate),
    Untrusted(UntrustedSkill),
}

fn read_candidate(
    root: &SkillRoot,
    rank: usize,
    dir: &Path,
    name: &str,
    trust: &DefinitionTrustView,
) -> CandidateRead {
    let invalid = |error: String| {
        CandidateRead::Candidate(Candidate {
            rank,
            name: name.to_string(),
            scope: root.scope,
            source: root.source,
            location: Some(dir.to_path_buf()),
            description: None,
            error: Some(error),
        })
    };
    let skill_md = dir.join("SKILL.md");
    let package = match leveler_definition_trust::read_skill_package(&skill_md) {
        Ok(package) => package,
        Err(error) => return invalid(error),
    };
    let Some(bytes) = package
        .iter()
        .find(|part| part.relative == "SKILL.md")
        .map(|part| part.bytes.clone())
    else {
        return invalid("missing SKILL.md".to_string());
    };
    if let Some(identity) = identity_for(root.scope, root.source, name) {
        let digest = DefinitionIdentity::skill_package_digest(&package);
        let decision = trust.check(&identity, &digest);
        if !decision.admits() {
            return CandidateRead::Untrusted(UntrustedSkill {
                name: name.to_string(),
                scope: root.scope,
                source: root.source,
                location: dir.to_path_buf(),
                identity: Some(identity.clone()),
                digest,
                decision,
            });
        }
    }
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
    CandidateRead::Candidate(Candidate {
        rank,
        name: name.to_string(),
        scope: root.scope,
        source: root.source,
        location: Some(dir.to_path_buf()),
        description: Some(description),
        error: None,
    })
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
    describe(root).summaries()
}

/// The full resolved registry for a project (the read model the CLI and TUI
/// render, and the one `discover`/`load` are derived from).
pub fn describe(root: &Path) -> SkillRegistry {
    // Client-side read model (TUI `/skills`, `$` popup). It reads the live
    // CodeLeveler home, because the display must make the same admission
    // decision the runtime makes for this user. A store that cannot be
    // resolved fails closed.
    let home = leveler_core::LevelerHome::resolve(leveler_core::environment());
    SkillRegistry::load(
        &SkillRoots::for_project(root),
        &DefinitionTrustView::load(home.root(), Some(root)),
    )
}

/// Load a skill's full body and structured bundled files by name.
pub fn load(root: &Path, name: &str) -> Option<SkillDetail> {
    describe(root).load_skill(name).ok()
}

/// Resolve `$name` mentions against every visible root.
pub fn resolve_mentions(root: &Path, text: &str) -> SkillMentionResolution {
    describe(root).resolve_mentions(text)
}
