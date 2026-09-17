//! Creating, updating and deleting agent definitions on disk.
//!
//! The one write path every surface uses — the CLI, the app protocol and the
//! model's authoring tools — so validation, path safety and atomicity live in
//! one place. A definition is two files; the registry must never see one
//! without the other:
//!
//! - **create** writes both files into a hidden staging directory next to the
//!   target, then renames the directory into place;
//! - **update** stages the complete new directory, moves the old one aside to
//!   a hidden name, renames the new one in, and removes the old one — between
//!   the two renames the agent is briefly absent, never half-written;
//! - **delete** renames the directory to a hidden name before removing it.
//!
//! The registry ignores hidden entries, so an interrupted mutation leaves at
//! most an invisible leftover, not a partial agent.

use std::io::Write as _;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use super::{
    AgentDefinition, AgentManifest, AgentRoots, AgentSource, INSTRUCTIONS_FILE, MANIFEST_FILE,
    is_reserved_agent_name, parse_manifest, project_agents_dir, render_manifest, resolve_manifest,
    validate_agent_name,
};

/// Where a definition is written.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum AgentScope {
    Project,
    User,
}

impl AgentScope {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Project => "project",
            Self::User => "user",
        }
    }

    fn source(self) -> AgentSource {
        match self {
            Self::Project => AgentSource::Project,
            Self::User => AgentSource::User,
        }
    }
}

/// Why a mutation was refused. Nothing was written.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentStoreError(pub String);

impl std::fmt::Display for AgentStoreError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for AgentStoreError {}

fn fail<T>(message: impl Into<String>) -> Result<T, AgentStoreError> {
    Err(AgentStoreError(message.into()))
}

/// The single write path for agent definitions.
pub struct AgentStore {
    roots: AgentRoots,
}

impl AgentStore {
    pub fn new(roots: AgentRoots) -> Self {
        Self { roots }
    }

    /// Create a new definition. Refused if one of that name already exists in
    /// the scope.
    pub fn create(
        &self,
        scope: AgentScope,
        manifest: &AgentManifest,
        instructions: &str,
    ) -> Result<AgentDefinition, AgentStoreError> {
        let name = checked_name(&manifest.name)?;
        let agents_dir = self.agents_dir(scope, true)?;
        let target = agents_dir.join(name);
        let def = validate(scope, manifest, instructions, &target)?;
        if std::fs::symlink_metadata(&target).is_ok() {
            return fail(format!(
                "{} agent \"{name}\" already exists at {}; update it instead",
                scope.as_str(),
                target.display()
            ));
        }
        let yaml = render_manifest(manifest).map_err(AgentStoreError)?;
        let staging = stage(&agents_dir, name, yaml.as_bytes(), instructions)?;
        if let Err(error) = std::fs::rename(&staging, &target) {
            let _ = std::fs::remove_dir_all(&staging);
            return fail(format!("cannot create {}: {error}", target.display()));
        }
        Ok(def)
    }

    /// Replace an existing definition in the scope. Files whose content does
    /// not change are carried over byte for byte, so a hand-written
    /// `agent.yaml` keeps its comments when only the instructions change; when
    /// nothing changes nothing is written.
    pub fn update(
        &self,
        scope: AgentScope,
        manifest: &AgentManifest,
        instructions: &str,
    ) -> Result<AgentDefinition, AgentStoreError> {
        let name = checked_name(&manifest.name)?;
        let agents_dir = self.agents_dir(scope, false)?;
        let target = agents_dir.join(name);
        existing_directory(&target, scope, name)?;
        let def = validate(scope, manifest, instructions, &target)?;

        let old_yaml = std::fs::read(target.join(MANIFEST_FILE)).ok();
        let old_instructions = std::fs::read_to_string(target.join(INSTRUCTIONS_FILE)).ok();
        let manifest_unchanged = old_yaml
            .as_deref()
            .and_then(|b| std::str::from_utf8(b).ok())
            .and_then(|t| parse_manifest(t).ok())
            .is_some_and(|old| old == *manifest);
        let instructions_unchanged = old_instructions.as_deref() == Some(instructions);
        if manifest_unchanged && instructions_unchanged {
            return Ok(def);
        }
        let yaml = match (manifest_unchanged, old_yaml) {
            (true, Some(bytes)) => bytes,
            _ => render_manifest(manifest)
                .map_err(AgentStoreError)?
                .into_bytes(),
        };
        let staging = stage(&agents_dir, name, &yaml, instructions)?;
        let aside = agents_dir.join(format!(".{name}.old-{}", unique()));
        if let Err(error) = std::fs::rename(&target, &aside) {
            let _ = std::fs::remove_dir_all(&staging);
            return fail(format!("cannot update {}: {error}", target.display()));
        }
        if let Err(error) = std::fs::rename(&staging, &target) {
            // Put the previous definition back rather than leave none.
            let _ = std::fs::rename(&aside, &target);
            let _ = std::fs::remove_dir_all(&staging);
            return fail(format!("cannot update {}: {error}", target.display()));
        }
        let _ = std::fs::remove_dir_all(&aside);
        Ok(def)
    }

    /// Remove a definition from the scope. Running children keep the snapshot
    /// they were spawned with; future spawns no longer find it here.
    pub fn delete(&self, scope: AgentScope, name: &str) -> Result<(), AgentStoreError> {
        let name = checked_name(name)?;
        let agents_dir = self.agents_dir(scope, false)?;
        let target = agents_dir.join(name);
        existing_directory(&target, scope, name)?;
        let aside = agents_dir.join(format!(".{name}.deleted-{}", unique()));
        std::fs::rename(&target, &aside)
            .or_else(|e| fail(format!("cannot delete {}: {e}", target.display())))?;
        std::fs::remove_dir_all(&aside).or_else(|e| {
            fail(format!(
                "agent \"{name}\" is no longer visible, but {} could not be removed: {e}",
                aside.display()
            ))
        })
    }

    /// The agents directory of `scope`, checked to be where it claims to be.
    fn agents_dir(&self, scope: AgentScope, create: bool) -> Result<PathBuf, AgentStoreError> {
        let (dir, containing_root) = match scope {
            AgentScope::Project => {
                let Some(root) = &self.roots.project_root else {
                    return fail("no project is open, so there is no project agents directory");
                };
                (project_agents_dir(root), Some(root.as_path()))
            }
            AgentScope::User => match &self.roots.user_agents_dir {
                Some(dir) => (dir.clone(), None),
                None => {
                    return fail(
                        "no CodeLeveler home is known, so there is no user agents directory",
                    );
                }
            },
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
                    "{} resolves outside the project; refusing to write agent definitions there",
                    dir.display()
                ));
            }
        }
        Ok(dir)
    }
}

fn checked_name(name: &str) -> Result<&str, AgentStoreError> {
    validate_agent_name(name).map_err(AgentStoreError)?;
    if is_reserved_agent_name(name) {
        return fail(format!(
            "\"{name}\" is a built-in agent with runtime structural meaning and cannot be \
             written; choose another name"
        ));
    }
    Ok(name)
}

fn validate(
    scope: AgentScope,
    manifest: &AgentManifest,
    instructions: &str,
    target: &Path,
) -> Result<AgentDefinition, AgentStoreError> {
    resolve_manifest(
        manifest,
        instructions,
        &manifest.name,
        scope.source(),
        Some(target.to_path_buf()),
    )
    .map_err(|e| AgentStoreError(format!("agent \"{}\" is invalid: {e}", manifest.name)))
}

fn existing_directory(target: &Path, scope: AgentScope, name: &str) -> Result<(), AgentStoreError> {
    match std::fs::symlink_metadata(target) {
        Ok(meta) if meta.file_type().is_dir() => Ok(()),
        Ok(_) => fail(format!(
            "{} is not a plain directory; refusing to modify it",
            target.display()
        )),
        Err(_) if super::builtin::is_builtin_name(name) => fail(format!(
            "\"{name}\" is a built-in agent and cannot be edited in place; create a {} agent \
             of another name from a copy of its definition",
            scope.as_str()
        )),
        Err(_) => fail(format!(
            "{} agent \"{name}\" does not exist",
            scope.as_str()
        )),
    }
}

/// Write both files, synced, into a fresh hidden directory beside the target.
fn stage(
    agents_dir: &Path,
    name: &str,
    manifest: &[u8],
    instructions: &str,
) -> Result<PathBuf, AgentStoreError> {
    let staging = agents_dir.join(format!(".{name}.staging-{}", unique()));
    let write = || -> std::io::Result<()> {
        std::fs::create_dir(&staging)?;
        for (file, bytes) in [
            (MANIFEST_FILE, manifest),
            (INSTRUCTIONS_FILE, instructions.as_bytes()),
        ] {
            let mut f = std::fs::File::create(staging.join(file))?;
            f.write_all(bytes)?;
            f.sync_all()?;
        }
        Ok(())
    };
    if let Err(error) = write() {
        let _ = std::fs::remove_dir_all(&staging);
        return fail(format!(
            "cannot write the agent definition under {}: {error}",
            agents_dir.display()
        ));
    }
    Ok(staging)
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
