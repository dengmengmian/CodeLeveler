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
    let store = normalize_absolute(&store);
    let project = normalize_absolute(&project);
    !(store == project
        || store
            .strip_prefix(&project)
            .is_some_and(|rest| rest.starts_with('/')))
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
}
