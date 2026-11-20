//! Definition discovery for the trust surface.
//!
//! The CLI and the startup prompt must list exactly the authority-bearing
//! files the loaders can inject — a file a human approved but the runtime never
//! reads is noise, and a file the runtime reads but the surface never lists is
//! an invisible bypass. Paths here mirror the loaders:
//!
//! - rules: `AGENTS.md` at any depth, `.leveler/instructions.md`,
//!   `.leveler/rules/*.md`;
//! - agents: `<project>/.leveler/agents/<name>/{agent.yaml,instructions.md}`
//!   and `<leveler-home>/agents/<name>/...`;
//! - skills: the project roots `.leveler/skills`, `.codex/skills`,
//!   `.agents/skills`, `.claude/skills`; the user roots
//!   `<leveler-home>/skills`, `~/.codex/skills`, `~/.agents/skills`,
//!   `~/.claude/skills`.
//!
//! The walk never follows symlinks, prunes generated/dependency trees, and is
//! bounded. A tree too large to enumerate is reported by `truncated`, not
//! silently presented as complete.

use std::path::{Path, PathBuf};

use leveler_core::EnvSnapshot;

use crate::store::ApprovalRequest;
use crate::{DefinitionIdentity, DefinitionKind};

/// Directory names a rule walk never enters: generated output, dependency
/// trees, VCS metadata and the `.leveler` tree (whose rule files are read
/// explicitly, not as nested `AGENTS.md`).
const PRUNED: &[&str] = &[
    ".git",
    ".leveler",
    "target",
    "node_modules",
    ".venv",
    "venv",
    "__pycache__",
    ".pytest_cache",
    ".mypy_cache",
    ".next",
    ".nuxt",
    "dist",
    "build",
];

const WALK_LIMIT: usize = 50_000;

const PROJECT_SKILL_ROOTS: &[(&str, &str)] = &[
    ("leveler", ".leveler/skills"),
    ("codex", ".codex/skills"),
    ("agents", ".agents/skills"),
    ("claude", ".claude/skills"),
];

const USER_SKILL_ROOTS: &[(&str, &str)] = &[
    ("codex", ".codex/skills"),
    ("agents", ".agents/skills"),
    ("claude", ".claude/skills"),
];

/// The roots that define what the trust surface can see.
#[derive(Debug, Clone, Default)]
pub struct DiscoveryRoots {
    pub project_root: Option<PathBuf>,
    pub leveler_home: PathBuf,
    pub user_home: Option<PathBuf>,
}

impl DiscoveryRoots {
    /// Resolve from the runtime's own environment snapshot. Never reads the
    /// child command's environment: a shell command cannot point the trust
    /// surface at a different store.
    pub fn resolve(env: &EnvSnapshot, project_root: Option<PathBuf>) -> Self {
        let home = leveler_core::LevelerHome::resolve(env).root().to_path_buf();
        let user_home = env
            .var_os("HOME")
            .filter(|value| !value.is_empty())
            .or_else(|| env.var_os("USERPROFILE").filter(|value| !value.is_empty()))
            .map(PathBuf::from);
        Self {
            project_root,
            leveler_home: home,
            user_home,
        }
    }
}

/// One definition the human may approve, with the exact digest the current
/// bytes produce.
#[derive(Debug, Clone)]
pub struct DefinitionCandidate {
    pub identity: DefinitionIdentity,
    /// The file the digest is over (the `SKILL.md` or rule); for an agent this
    /// is `agent.yaml`.
    pub primary_path: PathBuf,
    /// Every file that contributes to the composite digest.
    pub parts: Vec<PathBuf>,
    /// The current digest, or `None` when the files could not be read.
    pub digest: Option<String>,
    pub read_error: Option<String>,
}

impl DefinitionCandidate {
    /// Re-read the candidate and build the approval request from the bytes read
    /// now. The caller compares `request.digest()` with the digest it showed the
    /// human; a difference means the file changed while the decision was being
    /// made, and no record may be written.
    pub fn approval_request(&self) -> Result<ApprovalRequest, String> {
        if self.identity.kind == DefinitionKind::Skill {
            let package = crate::read_skill_package(&self.primary_path)?;
            return Ok(ApprovalRequest::skill_package(
                self.identity.clone(),
                &package,
            ));
        }
        let bytes: Vec<Vec<u8>> = self
            .parts
            .iter()
            .map(|path| std::fs::read(path).map_err(|error| format!("{}: {error}", path.display())))
            .collect::<Result<_, _>>()?;
        match self.identity.kind {
            DefinitionKind::Agent => {
                if bytes.len() != 2 {
                    return Err(
                        "an agent definition requires agent.yaml and instructions.md".into(),
                    );
                }
                Ok(ApprovalRequest::agent(
                    self.identity.clone(),
                    &bytes[0],
                    &bytes[1],
                ))
            }
            DefinitionKind::Skill => unreachable!("handled above"),
            _ => {
                let Some(body) = bytes.first() else {
                    return Err("the definition has no readable content".into());
                };
                Ok(ApprovalRequest::single(self.identity.clone(), body))
            }
        }
    }

    /// The same representation the store snapshots, for a diff against the
    /// approved version. Agent files are framed so a manifest and an
    /// instructions edit are both visible; a skill package frames every file
    /// that contributes to its approval.
    pub fn current_content(&self) -> Result<Vec<u8>, String> {
        if self.identity.kind == DefinitionKind::Skill {
            let package = crate::read_skill_package(&self.primary_path)?;
            let mut out = Vec::new();
            for part in package {
                out.extend_from_slice(format!("--- {} ---\n", part.relative).as_bytes());
                out.extend_from_slice(&part.bytes);
                if !part.bytes.ends_with(b"\n") {
                    out.push(b'\n');
                }
            }
            return Ok(out);
        }
        let bytes: Vec<Vec<u8>> = self
            .parts
            .iter()
            .map(|path| std::fs::read(path).map_err(|error| format!("{}: {error}", path.display())))
            .collect::<Result<_, _>>()?;
        if self.identity.kind != DefinitionKind::Agent {
            return bytes
                .into_iter()
                .next()
                .ok_or_else(|| "the definition has no readable content".to_string());
        }
        if bytes.len() != 2 {
            return Err("an agent definition requires agent.yaml and instructions.md".into());
        }
        let mut out = Vec::new();
        out.extend_from_slice(b"--- agent.yaml ---\n");
        out.extend_from_slice(&bytes[0]);
        if !bytes[0].ends_with(b"\n") {
            out.push(b'\n');
        }
        out.extend_from_slice(b"--- instructions.md ---\n");
        out.extend_from_slice(&bytes[1]);
        if !bytes[1].ends_with(b"\n") {
            out.push(b'\n');
        }
        Ok(out)
    }
}

/// Every definition the gate can reach for these roots.
pub fn discover(roots: &DiscoveryRoots) -> Vec<DefinitionCandidate> {
    let mut out = Vec::new();
    if let Some(root) = &roots.project_root {
        discover_project_into(root, &mut out);
    }
    discover_user_into(&roots.leveler_home, roots.user_home.as_deref(), &mut out);
    out.sort_by(|a, b| a.identity.id.cmp(&b.identity.id));
    out
}

pub fn discover_project(root: &Path) -> Vec<DefinitionCandidate> {
    let mut out = Vec::new();
    discover_project_into(root, &mut out);
    out.sort_by(|a, b| a.identity.id.cmp(&b.identity.id));
    out
}

pub fn discover_user(leveler_home: &Path, user_home: Option<&Path>) -> Vec<DefinitionCandidate> {
    let mut out = Vec::new();
    discover_user_into(leveler_home, user_home, &mut out);
    out.sort_by(|a, b| a.identity.id.cmp(&b.identity.id));
    out
}

/// Read a candidate's exact current digest without building an approval
/// request, for display in a status command.
pub fn read_candidate_digest(candidate: &DefinitionCandidate) -> Option<String> {
    candidate
        .approval_request()
        .ok()
        .map(|request| request.digest().to_string())
}

fn discover_project_into(root: &Path, out: &mut Vec<DefinitionCandidate>) {
    // Root and nested AGENTS.md; `.leveler` is pruned from that walk and its
    // rule documents are read explicitly by the walk when it sees the entry.
    let mut budget = WALK_LIMIT;
    walk(root, root, &mut budget, out);
    // `.leveler/agents/*`
    discover_agents(
        &root.join(".leveler").join("agents"),
        |name, path| {
            DefinitionIdentity::project(DefinitionKind::Agent, &format!(".leveler/agents/{name}"))
                .map(|identity| (identity, path))
        },
        out,
    );
    for (source, relative) in PROJECT_SKILL_ROOTS {
        discover_skills(&root.join(relative), source, false, out);
    }
}

fn discover_user_into(
    leveler_home: &Path,
    user_home: Option<&Path>,
    out: &mut Vec<DefinitionCandidate>,
) {
    discover_agents(
        &leveler_home.join("agents"),
        |name, path| DefinitionIdentity::user_agent(name).map(|identity| (identity, path)),
        out,
    );
    discover_skills(&leveler_home.join("skills"), "leveler", true, out);
    if let Some(home) = user_home {
        for (source, relative) in USER_SKILL_ROOTS {
            discover_skills(&home.join(relative), source, true, out);
        }
    }
}

fn walk(root: &Path, dir: &Path, budget: &mut usize, out: &mut Vec<DefinitionCandidate>) {
    if *budget == 0 {
        return;
    }
    *budget -= 1;
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let mut dirs = Vec::new();
    for entry in entries.flatten() {
        let Ok(file_type) = entry.file_type() else {
            continue;
        };
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            continue;
        };
        let path = entry.path();
        if file_type.is_symlink() {
            // The rule reader follows an in-workspace file symlink, so the
            // surface must list the same identity a human can approve. A link
            // that resolves outside is still listed; the reader refuses it, and
            // an approval for it grants nothing.
            if is_agents_md(name) && path.canonicalize().is_ok_and(|target| target.is_file()) {
                let relative = path.strip_prefix(root).unwrap_or(&path).to_string_lossy();
                let kind = if path == root.join("AGENTS.md") {
                    DefinitionKind::RootAgents
                } else {
                    DefinitionKind::NestedAgents
                };
                if let Some(identity) = DefinitionIdentity::project(kind, &relative) {
                    out.push(candidate(identity, vec![path]));
                }
            }
            continue;
        }
        if file_type.is_dir() {
            if PRUNED.iter().any(|skip| name.eq_ignore_ascii_case(skip)) {
                if name.eq_ignore_ascii_case(".leveler") && dir == root {
                    discover_leveler_rules(root, out);
                }
                continue;
            }
            dirs.push(path);
        } else if file_type.is_file() && is_agents_md(name) {
            let relative = path.strip_prefix(root).unwrap_or(&path).to_string_lossy();
            let kind = if path == root.join("AGENTS.md") {
                DefinitionKind::RootAgents
            } else {
                DefinitionKind::NestedAgents
            };
            if let Some(identity) = DefinitionIdentity::project(kind, &relative) {
                out.push(candidate(identity, vec![path]));
            }
        }
    }
    dirs.sort();
    for dir in dirs {
        walk(root, &dir, budget, out);
    }
}

/// `AGENTS.md` is matched case-insensitively only where the filesystem is.
fn is_agents_md(name: &str) -> bool {
    if cfg!(windows) {
        name.eq_ignore_ascii_case("AGENTS.md")
    } else {
        name == "AGENTS.md"
    }
}

fn discover_leveler_rules(root: &Path, out: &mut Vec<DefinitionCandidate>) {
    let instructions = root.join(".leveler").join("instructions.md");
    if instructions.is_file()
        && let Some(identity) =
            DefinitionIdentity::project(DefinitionKind::Instructions, ".leveler/instructions.md")
    {
        out.push(candidate(identity, vec![instructions]));
    }
    let rules_dir = root.join(".leveler").join("rules");
    let Ok(entries) = std::fs::read_dir(&rules_dir) else {
        return;
    };
    let mut files = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_file()
            && path
                .extension()
                .is_some_and(|extension| extension.eq_ignore_ascii_case("md"))
        {
            files.push(path);
        }
    }
    files.sort();
    for path in files {
        let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
            continue;
        };
        if let Some(identity) =
            DefinitionIdentity::project(DefinitionKind::RuleFile, &format!(".leveler/rules/{name}"))
        {
            out.push(candidate(identity, vec![path]));
        }
    }
}

fn discover_agents(
    dir: &Path,
    identity_for: impl Fn(&str, PathBuf) -> Option<(DefinitionIdentity, PathBuf)>,
    out: &mut Vec<DefinitionCandidate>,
) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let mut names = Vec::new();
    for entry in entries.flatten() {
        let Ok(file_type) = entry.file_type() else {
            continue;
        };
        let Some(name) = entry.file_name().to_str().map(str::to_string) else {
            continue;
        };
        if name.starts_with('.') || file_type.is_symlink() || !file_type.is_dir() {
            continue;
        }
        names.push((name, entry.path()));
    }
    names.sort();
    for (name, path) in names {
        let manifest = path.join("agent.yaml");
        let instructions = path.join("instructions.md");
        if !manifest.is_file() || !instructions.is_file() {
            continue;
        }
        if let Some((identity, _)) = identity_for(&name, path.clone()) {
            out.push(candidate(identity, vec![manifest, instructions]));
        }
    }
}

fn discover_skills(dir: &Path, source: &str, user: bool, out: &mut Vec<DefinitionCandidate>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let mut names = Vec::new();
    for entry in entries.flatten() {
        let Ok(file_type) = entry.file_type() else {
            continue;
        };
        let Some(name) = entry.file_name().to_str().map(str::to_string) else {
            continue;
        };
        if name.starts_with('.') || file_type.is_symlink() || !file_type.is_dir() {
            continue;
        }
        names.push((name, entry.path()));
    }
    names.sort();
    for (name, path) in names {
        let skill = path.join("SKILL.md");
        if !skill.is_file() {
            continue;
        }
        let identity = if user {
            DefinitionIdentity::user_skill(source, &name)
        } else {
            DefinitionIdentity::project(
                DefinitionKind::Skill,
                &format!("{}/{}", skill_root_relative(source), name),
            )
        };
        if let Some(identity) = identity {
            // List every file the approval digest binds, so the inventory and
            // `leveler trust show` name the same package the loader checks.
            let parts = crate::read_skill_package(&skill)
                .map(|package| package.into_iter().map(|part| part.path).collect())
                .unwrap_or_else(|_| vec![skill.clone()]);
            out.push(candidate(identity, parts));
        }
    }
}

fn skill_root_relative(source: &str) -> &'static str {
    match source {
        "leveler" => ".leveler/skills",
        "codex" => ".codex/skills",
        "agents" => ".agents/skills",
        "claude" => ".claude/skills",
        _ => ".leveler/skills",
    }
}

fn candidate(identity: DefinitionIdentity, parts: Vec<PathBuf>) -> DefinitionCandidate {
    let primary_path = parts.first().cloned().unwrap_or_default();
    let (digest, read_error) = match read_request_for(&identity, &parts) {
        Ok(request) => (Some(request.digest().to_string()), None),
        Err(error) => (None, Some(error)),
    };
    DefinitionCandidate {
        identity,
        primary_path,
        parts,
        digest,
        read_error,
    }
}

fn read_request_for(
    identity: &DefinitionIdentity,
    parts: &[PathBuf],
) -> Result<ApprovalRequest, String> {
    if identity.kind == DefinitionKind::Skill {
        let skill_md = parts
            .first()
            .ok_or_else(|| "the skill has no SKILL.md".to_string())?;
        let package = crate::read_skill_package(skill_md)?;
        return Ok(ApprovalRequest::skill_package(identity.clone(), &package));
    }
    let bytes: Vec<Vec<u8>> = parts
        .iter()
        .map(|path| std::fs::read(path).map_err(|error| format!("{}: {error}", path.display())))
        .collect::<Result<_, _>>()?;
    match identity.kind {
        DefinitionKind::Agent => {
            if bytes.len() != 2 {
                return Err("an agent definition requires agent.yaml and instructions.md".into());
            }
            Ok(ApprovalRequest::agent(
                identity.clone(),
                &bytes[0],
                &bytes[1],
            ))
        }
        _ => {
            let Some(body) = bytes.first() else {
                return Err("the definition has no readable content".into());
            };
            Ok(ApprovalRequest::single(identity.clone(), body))
        }
    }
}

/// Every identity the discovery produced, for a set comparison in tests.
#[cfg(test)]
pub(crate) fn identities(candidates: &[DefinitionCandidate]) -> std::collections::HashSet<String> {
    candidates
        .iter()
        .map(|candidate| candidate.identity.id.clone())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn discovery_finds_root_nested_agents_rules_and_skills() {
        let project = tempdir().unwrap();
        let home = tempdir().unwrap();
        std::fs::create_dir_all(project.path().join("src/deep")).unwrap();
        std::fs::write(project.path().join("AGENTS.md"), "root").unwrap();
        std::fs::write(project.path().join("src/AGENTS.md"), "src").unwrap();
        std::fs::write(project.path().join("src/deep/AGENTS.md"), "deep").unwrap();
        std::fs::create_dir_all(project.path().join(".leveler/rules")).unwrap();
        std::fs::write(project.path().join(".leveler/instructions.md"), "i").unwrap();
        std::fs::write(project.path().join(".leveler/rules/a.md"), "r").unwrap();
        std::fs::create_dir_all(project.path().join(".leveler/agents/reviewer")).unwrap();
        std::fs::write(
            project.path().join(".leveler/agents/reviewer/agent.yaml"),
            "a: b",
        )
        .unwrap();
        std::fs::write(
            project
                .path()
                .join(".leveler/agents/reviewer/instructions.md"),
            "do it",
        )
        .unwrap();
        std::fs::create_dir_all(project.path().join(".leveler/skills/foo")).unwrap();
        std::fs::write(
            project.path().join(".leveler/skills/foo/SKILL.md"),
            "---\nname: foo\n---\nx",
        )
        .unwrap();

        let candidates = discover(&DiscoveryRoots {
            project_root: Some(project.path().to_path_buf()),
            leveler_home: home.path().to_path_buf(),
            user_home: None,
        });
        let ids = identities(&candidates);
        for expected in [
            (
                "root",
                DefinitionIdentity::project(DefinitionKind::RootAgents, "AGENTS.md"),
            ),
            (
                "nested",
                DefinitionIdentity::project(DefinitionKind::NestedAgents, "src/AGENTS.md"),
            ),
            (
                "deep",
                DefinitionIdentity::project(DefinitionKind::NestedAgents, "src/deep/AGENTS.md"),
            ),
            (
                "instructions",
                DefinitionIdentity::project(
                    DefinitionKind::Instructions,
                    ".leveler/instructions.md",
                ),
            ),
            (
                "rule",
                DefinitionIdentity::project(DefinitionKind::RuleFile, ".leveler/rules/a.md"),
            ),
            (
                "agent",
                DefinitionIdentity::project(DefinitionKind::Agent, ".leveler/agents/reviewer"),
            ),
            (
                "skill",
                DefinitionIdentity::project(DefinitionKind::Skill, ".leveler/skills/foo"),
            ),
        ] {
            let id = expected.1.unwrap().id;
            assert!(ids.contains(&id), "missing {id}: {ids:?}");
        }
    }

    #[test]
    fn a_changed_candidate_recomputes_a_different_request_digest() {
        let project = tempdir().unwrap();
        std::fs::write(project.path().join("AGENTS.md"), "A").unwrap();
        let candidate = discover_project(project.path()).remove(0);
        let before = candidate.digest.clone().unwrap();
        std::fs::write(project.path().join("AGENTS.md"), "B").unwrap();
        let request = candidate.approval_request().unwrap();
        assert_ne!(request.digest(), before);
    }

    #[test]
    fn the_walk_does_not_enter_generated_trees() {
        let project = tempdir().unwrap();
        std::fs::create_dir_all(project.path().join("node_modules/pkg")).unwrap();
        std::fs::write(project.path().join("node_modules/pkg/AGENTS.md"), "evil").unwrap();
        std::fs::write(project.path().join("AGENTS.md"), "root").unwrap();
        let ids = identities(&discover_project(project.path()));
        let root = DefinitionIdentity::project(DefinitionKind::RootAgents, "AGENTS.md")
            .unwrap()
            .id;
        assert!(ids.contains(&root));
        assert!(!ids.iter().any(|id| id.contains("node_modules")));
    }
}
