//! Trusted definition admission.
//!
//! Project instruction files (`AGENTS.md` at any depth, `.leveler/instructions.md`,
//! `.leveler/rules/*.md`), declarative agents (`agent.yaml` + `instructions.md`)
//! and skills (`SKILL.md`) are *authority-bearing*: the harness puts them into
//! the model's control context. A sandboxed shell may be able to create such a
//! file in a workspace (Windows cannot name-fence a path that does not exist,
//! and creation of a missing `.leveler` directory is possible on every backend
//! that binds only existing protected paths). File existence therefore cannot
//! be the trust signal.
//!
//! The gate is content-addressed, exactly like the existing in-repo executable
//! config trust: a human approves a definition's exact bytes, and the store
//! records that digest. Any later byte change — by a teammate, `git pull`, the
//! agent, or a confined shell — invalidates the record. A missing store, a
//! corrupt store, a store that lives inside the project it grants for, and an
//! unreadable definition all resolve to *untrusted*. Nothing here trusts a
//! path because it exists, is git-tracked, or has a familiar name.
//!
//! Built-ins compiled into the binary are a separate source rule; callers do
//! not ask this gate about them.

mod discovery;
mod store;
mod view;

pub use discovery::{
    DefinitionCandidate, DiscoveryRoots, discover, discover_project, discover_user,
    read_candidate_digest,
};
pub use store::{
    ApprovalReport, ApprovalRequest, ApprovalSurface, DefinitionTrustStore, StoreLoad, StoreStatus,
    TrustStoreError, approve, read_snapshot, revoke_all, revoke_definition, store_path,
};
pub use view::{DefinitionTrust, DefinitionTrustView, TrustAuditEntry};

use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};

/// Schema version of the definition trust store.
pub const STORE_VERSION: u32 = 1;

/// The store file name under the Leveler home.
pub const STORE_FILE_NAME: &str = "definition-trust.yaml";

/// What kind of authority-bearing definition this is.
///
/// The kind is an audit label; identity lives in [`DefinitionIdentity`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DefinitionKind {
    /// The workspace-root `AGENTS.md`.
    RootAgents,
    /// An `AGENTS.md` below the workspace root.
    NestedAgents,
    /// `.leveler/instructions.md`.
    Instructions,
    /// `.leveler/rules/*.md`.
    RuleFile,
    /// One declarative agent (`agent.yaml` plus `instructions.md`).
    Agent,
    /// One skill (`SKILL.md`).
    Skill,
}

impl DefinitionKind {
    /// Stable store-key prefix. All rule documents share one namespace so the
    /// same file cannot be approved as one kind and loaded as another.
    pub fn key_prefix(self) -> &'static str {
        match self {
            Self::RootAgents | Self::NestedAgents | Self::Instructions | Self::RuleFile => "rule",
            Self::Agent => "agent",
            Self::Skill => "skill",
        }
    }

    /// Stable audit code (not a display string).
    pub fn as_str(self) -> &'static str {
        match self {
            Self::RootAgents => "root_agents",
            Self::NestedAgents => "nested_agents",
            Self::Instructions => "instructions",
            Self::RuleFile => "rule_file",
            Self::Agent => "agent",
            Self::Skill => "skill",
        }
    }
}

/// Where a definition came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DefinitionScope {
    /// Inside the workspace. A confined shell can reach these paths.
    Project,
    /// User configuration or user-wide compatibility roots. The execution
    /// sandbox does not grant writes there; the gate still binds the exact
    /// bytes so an editor change or an authoring tool does not inherit an old
    /// approval.
    User,
    /// Compiled into the binary. Never evaluated by this crate.
    Builtin,
}

impl DefinitionScope {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Project => "project",
            Self::User => "user",
            Self::Builtin => "builtin",
        }
    }
}

/// The stable identity of one authority-bearing definition.
///
/// The identity is the store key. It deliberately excludes the absolute path:
/// a project keeps its approvals when it moves, a copied tree does not inherit
/// another project's approvals (the domain key is the canonical project root),
/// and a path alias is a different identity unless it resolves to the same
/// approved key.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct DefinitionIdentity {
    pub scope: DefinitionScope,
    pub kind: DefinitionKind,
    /// Store key, e.g. `rule:src/AGENTS.md`, `agent:.leveler/agents/reviewer`,
    /// `skill:user:codex:foo`.
    pub id: String,
    /// Human-facing, workspace-relative (or user-root-relative) label.
    pub display: String,
}

impl DefinitionIdentity {
    /// A definition inside the workspace. `relative` is the workspace-relative
    /// path of the thing being approved: a rule document's path, an agent
    /// directory, or a skill directory.
    pub fn project(kind: DefinitionKind, relative: &str) -> Option<Self> {
        let display = clean_relative(relative)?;
        let normalized = if cfg!(windows) {
            display.to_lowercase()
        } else {
            display.clone()
        };
        Some(Self {
            scope: DefinitionScope::Project,
            kind,
            id: format!("{}:{normalized}", kind.key_prefix()),
            display,
        })
    }

    /// A user-scope agent under `<leveler-home>/agents/<name>`.
    pub fn user_agent(name: &str) -> Option<Self> {
        let display = clean_relative(name)?;
        let normalized = if cfg!(windows) {
            display.to_lowercase()
        } else {
            display.clone()
        };
        Some(Self {
            scope: DefinitionScope::User,
            kind: DefinitionKind::Agent,
            id: format!("agent:user:{normalized}"),
            display: format!("agents/{display}"),
        })
    }

    /// A user-scope skill. `source` is the ecosystem root label (`leveler`,
    /// `codex`, `agents`, `claude`).
    pub fn user_skill(source: &str, name: &str) -> Option<Self> {
        let display_name = clean_relative(name)?;
        let display_source = clean_relative(source)?;
        let normalized_name = if cfg!(windows) {
            display_name.to_lowercase()
        } else {
            display_name.clone()
        };
        let normalized_source = if cfg!(windows) {
            display_source.to_lowercase()
        } else {
            display_source.clone()
        };
        Some(Self {
            scope: DefinitionScope::User,
            kind: DefinitionKind::Skill,
            id: format!("skill:user:{normalized_source}:{normalized_name}"),
            display: format!("{display_source}/skills/{display_name}"),
        })
    }

    /// The digest form for a rule document or a skill's `SKILL.md`.
    pub fn single_digest(bytes: &[u8]) -> String {
        digest_with_domain(b"definition-v1\0rule-or-skill\0", &[bytes])
    }

    /// The composite digest of an agent definition. Both files are part of the
    /// identity, so a trusted manifest plus an untrusted instructions file can
    /// never run.
    pub fn agent_digest(manifest: &[u8], instructions: &[u8]) -> String {
        digest_with_domain(b"definition-v1\0agent\0", &[manifest, instructions])
    }

    /// The composite digest of a skill package: `SKILL.md` and every file under
    /// its `references/` tree.
    ///
    /// The single-file digest is deliberately NOT used for skills. The loader
    /// hands reference text to the model as part of the skill's procedure, so a
    /// reference edit after approval must invalidate the approval exactly like a
    /// `SKILL.md` edit. Scripts and assets stay ordinary tool resources: content
    /// binding says what the model may trust as procedure, not what may execute.
    pub fn skill_package_digest(parts: &[SkillPackagePart]) -> String {
        let mut hasher = Sha256::new();
        hasher.update(b"definition-v1\0skill-package\0");
        let mut order: Vec<usize> = (0..parts.len()).collect();
        order.sort_by(|&left, &right| parts[left].relative.cmp(&parts[right].relative));
        for index in order {
            let part = &parts[index];
            hasher.update((part.relative.len() as u64).to_le_bytes());
            hasher.update(part.relative.as_bytes());
            hasher.update((part.bytes.len() as u64).to_le_bytes());
            hasher.update(&part.bytes);
        }
        let hex = hasher
            .finalize()
            .iter()
            .fold(String::with_capacity(64), |mut out, byte| {
                use std::fmt::Write as _;
                let _ = write!(out, "{byte:02x}");
                out
            });
        format!("sha256:{hex}")
    }
}

/// One file that contributes to a skill package approval.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkillPackagePart {
    /// Package-relative path with `/` separators (`SKILL.md`,
    /// `references/guide.md`).
    pub relative: String,
    pub path: std::path::PathBuf,
    pub bytes: Vec<u8>,
}

/// Longest reference tree the trust gate will hash or hand to the model.
///
/// A package is human-approved instruction text, not an asset store. Bounding
/// it keeps one skill from exhausting the tool result budget while still
/// leaving room for the long references real skills ship. Scripts and assets
/// are excluded from the bound because they are not content-admitted.
pub const MAX_SKILL_PACKAGE_FILES: usize = 512;
pub const MAX_SKILL_PACKAGE_BYTES: usize = 32 * 1024 * 1024;

/// Read the exact bytes that make up a skill's approval: `SKILL.md` plus every
/// regular file under `references/`, recursively.
///
/// The walk is deterministic (sorted by package-relative path), never follows a
/// symlink, and rejects anything that is not a regular UTF-8 file. A symlinked
/// or dangling reference would otherwise let the approved manifest name bytes
/// that can change behind the digest. The result always contains `SKILL.md`
/// first.
pub fn read_skill_package(skill_md: &std::path::Path) -> Result<Vec<SkillPackagePart>, String> {
    let Some(dir) = skill_md.parent() else {
        return Err("the skill path has no package directory".into());
    };
    let mut parts = Vec::new();
    let mut total = 0usize;

    // A symlinked SKILL.md may name a real file inside the package (the
    // existing loader supports that), but it may not point outside: the digest
    // must bind a file this package owns.
    let skill_meta =
        std::fs::symlink_metadata(skill_md).map_err(|error| format!("SKILL.md: {error}"))?;
    if skill_meta.file_type().is_symlink()
        && let (Ok(target), Ok(base)) = (skill_md.canonicalize(), dir.canonicalize())
        && !target.starts_with(&base)
    {
        return Err("SKILL.md is a symlink that points outside the skill directory".into());
    }
    let skill_bytes = read_regular_utf8(skill_md, "SKILL.md")?;
    total = total.saturating_add(skill_bytes.len());
    parts.push(SkillPackagePart {
        relative: "SKILL.md".to_string(),
        path: skill_md.to_path_buf(),
        bytes: skill_bytes,
    });

    let references = dir.join("references");
    match std::fs::symlink_metadata(&references) {
        Ok(metadata) => {
            if metadata.file_type().is_symlink() {
                return Err("the skill `references` directory is a symlink".into());
            }
            if !metadata.is_dir() {
                return Err("the skill `references` path is not a directory".into());
            }
            walk_skill_references(&references, &references, &mut parts, &mut total)?;
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(format!("{}: {error}", references.display())),
    }

    parts.sort_by(|left, right| left.relative.cmp(&right.relative));
    if parts.first().is_none_or(|part| part.relative != "SKILL.md") {
        return Err("the skill package has no readable SKILL.md".into());
    }
    Ok(parts)
}

fn walk_skill_references(
    root: &std::path::Path,
    current: &std::path::Path,
    parts: &mut Vec<SkillPackagePart>,
    total: &mut usize,
) -> Result<(), String> {
    let entries =
        std::fs::read_dir(current).map_err(|error| format!("{}: {error}", current.display()))?;
    let mut listed = Vec::new();
    for entry in entries {
        listed.push(entry.map_err(|error| format!("{}: {error}", current.display()))?);
    }
    listed.sort_by_key(|entry| entry.file_name());
    for entry in listed {
        let path = entry.path();
        let metadata = std::fs::symlink_metadata(&path)
            .map_err(|error| format!("{}: {error}", path.display()))?;
        let file_type = metadata.file_type();
        if file_type.is_symlink() {
            return Err(format!(
                "skill reference {} is a symlink; references must be regular files inside the package",
                path.display()
            ));
        }
        if file_type.is_dir() {
            walk_skill_references(root, &path, parts, total)?;
            continue;
        }
        if !file_type.is_file() {
            return Err(format!(
                "skill reference {} is not a regular file",
                path.display()
            ));
        }
        let relative = path
            .strip_prefix(root)
            .map_err(|_| format!("{} escapes the references directory", path.display()))?
            .to_str()
            .ok_or_else(|| {
                format!(
                    "skill reference {} has a non-UTF-8 package-relative name",
                    path.display()
                )
            })?
            .replace('\\', "/");
        let relative = format!("references/{relative}");
        if parts.len() >= MAX_SKILL_PACKAGE_FILES {
            return Err(format!(
                "the skill has more than {MAX_SKILL_PACKAGE_FILES} approved files"
            ));
        }
        if metadata.len() as usize > MAX_SKILL_PACKAGE_BYTES.saturating_sub(*total) {
            return Err(format!(
                "the skill's approved content exceeds {MAX_SKILL_PACKAGE_BYTES} bytes"
            ));
        }
        let bytes = read_regular_utf8(&path, &relative)?;
        *total = total.saturating_add(bytes.len());
        if *total > MAX_SKILL_PACKAGE_BYTES {
            return Err(format!(
                "the skill's approved content exceeds {MAX_SKILL_PACKAGE_BYTES} bytes"
            ));
        }
        parts.push(SkillPackagePart {
            relative,
            path,
            bytes,
        });
    }
    Ok(())
}

fn read_regular_utf8(path: &std::path::Path, label: &str) -> Result<Vec<u8>, String> {
    let metadata = std::fs::metadata(path).map_err(|error| format!("{label}: {}", error))?;
    if !metadata.is_file() {
        return Err(format!("{label}: not a regular file"));
    }
    if metadata.len() > MAX_SKILL_PACKAGE_BYTES as u64 {
        return Err(format!(
            "{label}: larger than the {MAX_SKILL_PACKAGE_BYTES}-byte package budget"
        ));
    }
    let bytes = std::fs::read(path).map_err(|error| format!("{label}: {error}"))?;
    if std::str::from_utf8(&bytes).is_err() {
        return Err(format!("{label}: not valid UTF-8"));
    }
    Ok(bytes)
}

/// Normalize a workspace-relative path or name for use in a store key.
///
/// Rejects absolute paths, drive prefixes and `..`; collapses `.` and repeated
/// separators; uses `/` everywhere. On Windows the key is case-folded because
/// the filesystem is case-insensitive — the same file must not gain a second
/// identity by changing a letter's case.
pub fn normalize_relative(relative: &str) -> Option<String> {
    let clean = clean_relative(relative)?;
    Some(if cfg!(windows) {
        clean.to_lowercase()
    } else {
        clean
    })
}

/// Validate and collapse a relative path, preserving the author's case.
pub fn clean_relative(relative: &str) -> Option<String> {
    let relative = relative.replace('\\', "/");
    // A leading separator is absolute on POSIX, and the two-separator form is
    // a UNC share on Windows. Neither is a workspace-relative name.
    if relative.starts_with('/') {
        return None;
    }
    let mut parts = Vec::new();
    for part in relative.split('/') {
        match part {
            "" | "." => continue,
            ".." => return None,
            _ => {
                if part.contains('\0') {
                    return None;
                }
                parts.push(part);
            }
        }
    }
    if parts.is_empty() {
        return None;
    }
    let joined = parts.join("/");
    // A Windows drive (`C:`) or UNC prefix is absolute, not relative.
    if joined.len() >= 2 && joined.as_bytes()[1] == b':' {
        return None;
    }
    Some(joined)
}

fn digest_with_domain(domain: &[u8], parts: &[&[u8]]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(domain);
    for part in parts {
        hasher.update((part.len() as u64).to_le_bytes());
        hasher.update(part);
    }
    let hex = hasher
        .finalize()
        .iter()
        .fold(String::with_capacity(64), |mut out, byte| {
            use std::fmt::Write as _;
            let _ = write!(out, "{byte:02x}");
            out
        });
    format!("sha256:{hex}")
}

/// The domain key a project's approvals are stored under. Canonicalized so
/// `.`, relative and symlinked spellings of one checkout agree; case-folded on
/// Windows for the same reason as relative paths.
pub fn project_domain(root: &std::path::Path) -> String {
    let canonical = root.canonicalize().unwrap_or_else(|_| root.to_path_buf());
    normalize_absolute(&canonical)
}

/// The user domain is one global namespace: user agents and user-wide skills
/// follow the person, not a project.
pub const USER_DOMAIN: &str = "user";

/// Resolve a possibly relative path against the process cwd before comparing.
/// A relative `LEVELER_HOME` otherwise compares as "outside" every absolute
/// project even when it lands inside the current checkout.
fn resolve_for_compare(path: &std::path::Path) -> std::path::PathBuf {
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .map(|cwd| cwd.join(path))
            .unwrap_or_else(|_| path.to_path_buf())
    };
    absolute.canonicalize().unwrap_or(absolute)
}

fn normalize_absolute(path: &std::path::Path) -> String {
    let text = path.to_string_lossy().replace('\\', "/");
    let text = text.trim_end_matches('/');
    if cfg!(windows) {
        text.to_lowercase()
    } else {
        text.to_string()
    }
}

/// Compare spelling for the trust-store namespace. `std::fs::canonicalize`
/// returns a Windows verbatim `\\?\` path while an agent-supplied spelling
/// does not; stripping the marker here keeps the store identity stable without
/// changing the persisted project-domain keys.
fn compare_absolute(path: &std::path::Path) -> String {
    let text = path.to_string_lossy().replace('\\', "/");
    let text = text.trim_end_matches('/');
    let text = if cfg!(windows) {
        if let Some(rest) = text.strip_prefix("//?/UNC/") {
            format!("//{rest}")
        } else {
            text.strip_prefix("//?/").unwrap_or(text).to_string()
        }
    } else {
        text.to_string()
    };
    if cfg!(windows) {
        text.to_lowercase()
    } else {
        text
    }
}

/// Whether `store_root` (the Leveler home) is outside `project_root`. A store
/// inside the project could be authored by the same sandboxed shell the gate
/// defends against, so such an arrangement grants nothing.
pub fn store_is_outside_project(
    store_root: &std::path::Path,
    project_root: &std::path::Path,
) -> bool {
    let store = resolve_for_compare(store_root);
    let project = resolve_for_compare(project_root);
    if !cfg!(windows) {
        return !store.starts_with(&project);
    }
    let store = compare_absolute(&store);
    let project = compare_absolute(&project);
    !(store == project
        || store
            .strip_prefix(&project)
            .is_some_and(|rest| rest.starts_with('/')))
}

/// Whether `candidate` names the definition trust store, its lock file, or a
/// file under its snapshot directory.
///
/// Tool-facing writes must never author these files: a forged approval record
/// is equivalent to a human approval. The comparison resolves an existing
/// candidate and the candidate's parent, so a junction/symlink alias inside a
/// writable workspace cannot spell the store as an innocent relative path.
pub fn is_trust_store_path(store_root: &std::path::Path, candidate: &std::path::Path) -> bool {
    let store = resolve_for_compare(store_root);
    let candidate = resolve_candidate_for_compare(candidate);
    if paths_equal(&candidate, &store.join(STORE_FILE_NAME))
        || paths_equal(&candidate, &crate::store::lock_path(&store))
    {
        return true;
    }
    if path_is_under(&crate::store::snapshot_root(&store), &candidate) {
        return true;
    }
    // Atomic writes leave a `.definition-trust.yaml.tmp-*` beside the store.
    // No tool may author one: a later rename would make it the store.
    candidate
        .parent()
        .is_some_and(|parent| paths_equal(parent, &store))
        && candidate
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| name.starts_with(&format!(".{STORE_FILE_NAME}.tmp-")))
}

/// Whether `candidate` is, is under, or contains the runtime's local-control
/// namespace (`<home>/run/sockets`).
///
/// That namespace holds the daemon's access token, ownership lock and Unix
/// socket. A sandboxed agent must not read the token (it could authenticate as
/// a trusted local client) and must not be able to search a parent directory
/// that recursively exposes it. File tools call this before any content read;
/// the OS sandbox masks the same directory for child commands.
pub fn is_runtime_control_path(home_root: &std::path::Path, candidate: &std::path::Path) -> bool {
    let control = resolve_for_compare(home_root).join("run").join("sockets");
    let candidate = resolve_candidate_for_compare(candidate);
    path_is_under(&control, &candidate) || path_is_under(&candidate, &control)
}

fn resolve_candidate_for_compare(path: &std::path::Path) -> std::path::PathBuf {
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .map(|cwd| cwd.join(path))
            .unwrap_or_else(|_| path.to_path_buf())
    };
    if let Ok(canonical) = absolute.canonicalize() {
        return canonical;
    }
    // A store file that does not exist yet still has a parent that may be a
    // junction or symlink. Resolve that part before comparing the final name.
    if let (Some(parent), Some(name)) = (absolute.parent(), absolute.file_name())
        && let Ok(canonical_parent) = parent.canonicalize()
    {
        return canonical_parent.join(name);
    }
    absolute
}

fn paths_equal(left: &std::path::Path, right: &std::path::Path) -> bool {
    if cfg!(windows) {
        compare_absolute(left) == compare_absolute(right)
    } else {
        left == right
    }
}

fn path_is_under(root: &std::path::Path, candidate: &std::path::Path) -> bool {
    if cfg!(windows) {
        let root = compare_absolute(root);
        let candidate = compare_absolute(candidate);
        candidate == root
            || candidate
                .strip_prefix(&root)
                .is_some_and(|rest| rest.starts_with('/'))
    } else {
        candidate.starts_with(root)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn relative_identities_reject_escapes_and_absolute_paths() {
        assert!(normalize_relative("../AGENTS.md").is_none());
        assert!(normalize_relative("a/../../AGENTS.md").is_none());
        assert!(normalize_relative("C:/Windows/AGENTS.md").is_none());
        assert!(normalize_relative("/etc/AGENTS.md").is_none());
        assert_eq!(
            normalize_relative("./src//AGENTS.md").as_deref(),
            Some(if cfg!(windows) {
                "src/agents.md"
            } else {
                "src/AGENTS.md"
            })
        );
    }

    #[test]
    fn agent_digest_covers_both_files_and_resists_concatenation() {
        let a = DefinitionIdentity::agent_digest(b"m1", b"i1");
        assert_ne!(a, DefinitionIdentity::agent_digest(b"m1", b"i2"));
        assert_ne!(a, DefinitionIdentity::agent_digest(b"m2", b"i1"));
        // Length framing keeps ("ab","c") distinct from ("a","bc").
        assert_ne!(
            DefinitionIdentity::agent_digest(b"ab", b"c"),
            DefinitionIdentity::agent_digest(b"a", b"bc")
        );
    }

    #[test]
    fn user_identities_are_global_and_source_tagged() {
        let codex = DefinitionIdentity::user_skill("codex", "foo").unwrap();
        let native = DefinitionIdentity::user_skill("leveler", "foo").unwrap();
        assert_ne!(codex.id, native.id);
        assert_eq!(codex.scope, DefinitionScope::User);
        assert!(DefinitionIdentity::user_agent("reviewer").is_some());
    }

    #[test]
    fn a_skill_package_digest_binds_names_and_contents() {
        let plain = vec![
            SkillPackagePart {
                relative: "SKILL.md".into(),
                path: "SKILL.md".into(),
                bytes: b"body".to_vec(),
            },
            SkillPackagePart {
                relative: "references/a.md".into(),
                path: "references/a.md".into(),
                bytes: b"A".to_vec(),
            },
        ];
        let mut renamed = plain.clone();
        renamed[1].relative = "references/b.md".into();
        let mut changed = plain.clone();
        changed[1].bytes = b"B".to_vec();
        let baseline = DefinitionIdentity::skill_package_digest(&plain);
        assert_ne!(baseline, DefinitionIdentity::skill_package_digest(&renamed));
        assert_ne!(baseline, DefinitionIdentity::skill_package_digest(&changed));
        // Framing keeps a reference that starts like the previous one distinct.
        let split = vec![
            SkillPackagePart {
                relative: "references/a".into(),
                path: "references/a".into(),
                bytes: b"bc".to_vec(),
            },
            SkillPackagePart {
                relative: "references/b".into(),
                path: "references/b".into(),
                bytes: b"".to_vec(),
            },
        ];
        let joined = vec![
            SkillPackagePart {
                relative: "references/a".into(),
                path: "references/a".into(),
                bytes: b"b".to_vec(),
            },
            SkillPackagePart {
                relative: "references/b".into(),
                path: "references/b".into(),
                bytes: b"c".to_vec(),
            },
        ];
        assert_ne!(
            DefinitionIdentity::skill_package_digest(&split),
            DefinitionIdentity::skill_package_digest(&joined)
        );
    }

    #[test]
    fn reading_a_skill_package_includes_nested_references() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("SKILL.md"), "---\nname: x\n---\nbody\n").unwrap();
        std::fs::create_dir_all(dir.path().join("references/nested")).unwrap();
        std::fs::write(dir.path().join("references/a.md"), "A").unwrap();
        std::fs::write(dir.path().join("references/nested/b.md"), "B").unwrap();
        let package = read_skill_package(&dir.path().join("SKILL.md")).unwrap();
        let names: Vec<_> = package.iter().map(|part| part.relative.as_str()).collect();
        assert_eq!(
            names,
            vec!["SKILL.md", "references/a.md", "references/nested/b.md"]
        );
        assert_eq!(package[0].bytes, b"---\nname: x\n---\nbody\n");
    }

    #[cfg(unix)]
    #[test]
    fn a_symlinked_reference_is_refused() {
        use std::os::unix::fs::symlink;
        let dir = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("SKILL.md"), "---\nname: x\n---\nbody\n").unwrap();
        std::fs::write(outside.path().join("secret.md"), "SECRET").unwrap();
        std::fs::create_dir_all(dir.path().join("references")).unwrap();
        symlink(
            outside.path().join("secret.md"),
            dir.path().join("references/alias.md"),
        )
        .unwrap();
        let error = read_skill_package(&dir.path().join("SKILL.md")).unwrap_err();
        assert!(error.contains("symlink"), "{error}");
    }
}

#[cfg(test)]
mod path_boundary_tests {
    use super::*;

    #[test]
    fn a_relative_store_inside_the_project_is_refused() {
        let base = std::path::PathBuf::from("target")
            .join(format!("leveler-relative-trust-{}", std::process::id()));
        let project = base.join("project");
        let store = project.join("nested-home");
        std::fs::create_dir_all(project.join("nested-home")).unwrap();
        assert!(
            !store_is_outside_project(&store, &project),
            "a relative store that lands inside the project must be refused"
        );
        // The same spelling for a sibling is outside.
        let sibling = base.join("other-home");
        std::fs::create_dir_all(&sibling).unwrap();
        assert!(store_is_outside_project(&sibling, &project));
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn a_missing_store_inside_the_project_is_still_refused() {
        let project = tempfile::tempdir().unwrap();
        let store = project.path().join("not-created-yet");
        assert!(!store_is_outside_project(&store, project.path()));
        let view = DefinitionTrustView::load(&store, Some(project.path()));
        assert_eq!(view.status(), crate::store::StoreStatus::InsideProject);
    }

    #[test]
    fn the_runtime_control_namespace_is_recognized_in_both_directions() {
        let home = tempfile::tempdir().unwrap();
        let sockets = home.path().join("run/sockets");
        assert!(is_runtime_control_path(home.path(), &sockets));
        assert!(is_runtime_control_path(
            home.path(),
            &sockets.join("repo.sock")
        ));
        assert!(is_runtime_control_path(
            home.path(),
            &sockets.join("repo.token")
        ));
        // A directory the token could be searched from names the boundary too.
        assert!(is_runtime_control_path(home.path(), home.path()));
        assert!(is_runtime_control_path(
            home.path(),
            &home.path().join("run")
        ));
        assert!(!is_runtime_control_path(
            home.path(),
            &home.path().join("cache/tools")
        ));
        assert!(!is_runtime_control_path(
            home.path(),
            &home.path().join("run/sandboxes/x")
        ));
    }

    #[test]
    fn the_definition_store_namespace_is_recognized() {
        let home = tempfile::tempdir().unwrap();
        let store = home.path().join(STORE_FILE_NAME);
        assert!(is_trust_store_path(home.path(), &store));
        assert!(is_trust_store_path(
            home.path(),
            &home.path().join("definition-trust.lock")
        ));
        assert!(is_trust_store_path(
            home.path(),
            &home.path().join("definition-trust/snapshots/abc")
        ));
        assert!(is_trust_store_path(
            home.path(),
            &home.path().join(format!(".{STORE_FILE_NAME}.tmp-1-2"))
        ));
        assert!(!is_trust_store_path(
            home.path(),
            &home.path().join("config.toml")
        ));
        assert!(!is_trust_store_path(
            home.path(),
            &home.path().join("other/definition-trust.yaml")
        ));
    }
}
