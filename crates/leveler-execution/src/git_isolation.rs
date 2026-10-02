//! Frozen, process-local execution target for grant-eligible remote Git
//! commands (`fetch`, `ls-remote`, `push`).
//!
//! Why this exists: an approved `ConfiguredRemote` binding freezes the canonical
//! URL, but Git resolves its remote again when it starts — reading
//! `.git/config`, the global/system config, environment overrides, URL rewrites,
//! remote helpers and hooks. Rechecking right before `git` runs is a TOCTOU, not
//! a binding. Instead the execution layer builds a private git directory whose
//! `config` IS the bytes read once and verified, neutralizes the global/system
//! config and hooks, and pins the remote URL on the command line. The spawned
//! Git process therefore cannot resolve the approved target to anything else.
//!
//! The private directory keeps the real repository's `objects`/`refs`/`logs`
//! reachable through directory symlinks, so the approved effect still lands on
//! the repository admission bound. `HEAD` is copied: Git validates it as a
//! regular file, and for `fetch`/`ls-remote`/`push` it is read-only.

use leveler_core::{Capability, EnvSnapshot, GrantRequest, ResourceIdentity};
use std::path::{Path, PathBuf};

/// The immutable facts one approved remote Git effect is bound to. Recovered
/// from the matched grant, never from a live re-resolution of the remote name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApprovedGitTarget {
    /// Approved repository identity (same value admission bound).
    pub repository: String,
    /// Configured remote whose URL approval covers this effect.
    pub remote_name: String,
    /// Effective canonical URL, without credentials.
    pub canonical_url: String,
    /// Transport implied by the URL scheme.
    pub transport: String,
    /// Whether this call writes to the remote.
    pub write: bool,
    /// The approved credential incarnation for this destination, when the
    /// grant carried one. Its value is never stored here — only the host and
    /// the one-way identity commitment the execution layer must re-verify.
    pub credential: Option<ApprovedCredential>,
}

/// The exact credential a remote effect may use, without its value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApprovedCredential {
    /// Destination host the credential is bound to.
    pub host: String,
    /// One-way commitment to the credential incarnation admission approved.
    pub identity: String,
    /// Transport the credential is valid for.
    pub transport: String,
}

/// Remote Git commands whose effect a frozen target can cover. Anything else
/// keeps exact-call approval rather than widening the grant surface.
pub(crate) fn git_remote_operation(program: &str, args: &[String]) -> Option<GitRemoteOperation> {
    if std::path::Path::new(program)
        .file_name()
        .and_then(|name| name.to_str())
        != Some("git")
    {
        return None;
    }
    match args.first().map(String::as_str)? {
        "fetch" => Some(GitRemoteOperation::Fetch),
        "ls-remote" => Some(GitRemoteOperation::LsRemote),
        "push" => Some(GitRemoteOperation::Push),
        _ => None,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum GitRemoteOperation {
    Fetch,
    LsRemote,
    Push,
}

impl GitRemoteOperation {
    fn write(self) -> bool {
        matches!(self, Self::Push)
    }
}

/// Config keys that change which remote, transport, hook or credential program a
/// grant-eligible Git command actually uses. One shared list keeps admission and
/// execution from drifting apart. `workspace_mutation` adds the keys that only
/// matter when the same call also rewrites the worktree (filter/diff drivers).
pub(crate) fn effect_critical_config_key(key: &str, workspace_mutation: bool) -> bool {
    let key = key.to_ascii_lowercase();
    (key.starts_with("url.") && (key.ends_with(".insteadof") || key.ends_with(".pushinsteadof")))
        || key.starts_with("submodule.")
        || key == "fetch.recursesubmodules"
        || key == "push.recursesubmodules"
        || key == "core.sshcommand"
        || key == "core.hookspath"
        || key == "core.fsmonitor"
        || (key.starts_with("http.")
            && (key.ends_with(".proxy")
                || key.ends_with(".extraheader")
                || key.ends_with(".sslcert")
                || key.ends_with(".sslkey")
                || key.ends_with(".cookiefile")))
        || (workspace_mutation
            && (key.starts_with("filter.")
                || (key.starts_with("diff.")
                    && (key.ends_with(".command") || key.ends_with(".textconv")))))
        || key.starts_with("include")
        || (key.starts_with("remote.")
            && (key.ends_with(".uploadpack")
                || key.ends_with(".receivepack")
                || key.ends_with(".proxy")
                || key.ends_with(".vcs")))
}

/// Whether a parsed config activates any credential helper. An empty value
/// resets the accumulated list, exactly as Git applies it.
pub(crate) fn has_active_credential_helper(config: &[(&str, &str)]) -> bool {
    let mut helpers = std::collections::BTreeMap::<String, Vec<&str>>::new();
    for (key, value) in config {
        let lower = key.to_ascii_lowercase();
        if lower == "credential.helper"
            || (lower.starts_with("credential.") && lower.ends_with(".helper"))
        {
            let active = helpers.entry((*key).to_string()).or_default();
            if value.is_empty() {
                active.clear();
            } else {
                active.push(*value);
            }
        }
    }
    helpers.values().any(|values| !values.is_empty())
}

/// Recover the approved target for this call from a matched grant. `None` when
/// the call is not a grant-eligible remote Git command. An inconsistent grant
/// (no matching binding, or two different targets) is an error, never a silent
/// pass.
pub fn approved_git_target(
    grant: &GrantRequest,
    program: &str,
    args: &[String],
) -> Result<Option<ApprovedGitTarget>, String> {
    let Some(operation) = git_remote_operation(program, args) else {
        return Ok(None);
    };
    let mut argv = Vec::with_capacity(args.len() + 1);
    argv.push(program.to_string());
    argv.extend(args.iter().cloned());
    let Some(selection) = crate::git_effects::git_grant_selection(&argv) else {
        return Err("approved remote command lost its resolved shape".into());
    };
    if selection.effects.remote_write != operation.write() {
        return Err("approved remote command changed its effect class".into());
    }
    let Some(remote_name) = selection.remote_name else {
        return Ok(None);
    };
    let capability = if !operation.write() {
        Capability::RemoteRead
    } else if selection.effects.irreversible {
        Capability::RemoteForce
    } else {
        Capability::RemoteMutate
    };
    let mut found: Option<ApprovedGitTarget> = None;
    let mut credential: Option<ApprovedCredential> = None;
    for binding in &grant.bindings {
        if let (
            Capability::CredentialUse,
            ResourceIdentity::Credential {
                project,
                host,
                identity,
                transport,
            },
        ) = (&binding.capability, &binding.resource)
        {
            if project != &grant.project_identity {
                return Err("approved credential belongs to another project".into());
            }
            let candidate = ApprovedCredential {
                host: host.clone(),
                identity: identity.clone(),
                transport: transport.clone(),
            };
            match &credential {
                Some(existing) if existing != &candidate => {
                    return Err("ambiguous approved credential".into());
                }
                Some(_) => {}
                None => credential = Some(candidate),
            }
        }
    }
    for binding in &grant.bindings {
        let ResourceIdentity::ConfiguredRemote {
            repository,
            remote_name: bound_name,
            canonical_url,
            transport,
        } = &binding.resource
        else {
            continue;
        };
        if binding.capability != capability || bound_name != &remote_name {
            continue;
        }
        if repository != &grant.project_identity {
            return Err("approved remote belongs to another project".into());
        }
        let target = ApprovedGitTarget {
            repository: repository.clone(),
            remote_name: remote_name.clone(),
            canonical_url: canonical_url.clone(),
            transport: transport.clone(),
            write: operation.write(),
            credential: credential.clone(),
        };
        match &found {
            Some(existing) if existing != &target => {
                return Err("ambiguous approved Git target".into());
            }
            Some(_) => {}
            None => found = Some(target),
        }
    }
    // A credential binding is only meaningful attached to the destination it
    // was approved for; host, transport or project drift is a refusal.
    if let (Some(target), Some(credential)) = (&found, &credential) {
        let host = url::Url::parse(&target.canonical_url)
            .ok()
            .and_then(|url| url.host_str().map(str::to_string));
        if host.as_deref() != Some(credential.host.as_str())
            || credential.transport != target.transport
        {
            return Err("approved credential does not match the approved destination".into());
        }
    } else if found.is_none() && credential.is_some() {
        return Err("approved credential has no approved remote effect".into());
    }
    Ok(found)
}

/// A prepared, isolated Git invocation. Owns its private git directory; dropping
/// removes it unless [`GitIsolation::keep`] handed it to a detached process.
pub struct GitIsolation {
    program: String,
    args: Vec<String>,
    authority_env: Vec<(String, String)>,
    deny_env: Vec<String>,
    root: PathBuf,
    cleanup: bool,
    credential: bool,
}

impl Drop for GitIsolation {
    fn drop(&mut self) {
        if self.cleanup {
            let _ = std::fs::remove_dir_all(&self.root);
        }
    }
}

impl GitIsolation {
    pub fn program(&self) -> &str {
        &self.program
    }

    pub fn args(&self) -> &[String] {
        &self.args
    }

    pub fn authority_env(&self) -> &[(String, String)] {
        &self.authority_env
    }

    pub fn deny_env(&self) -> &[String] {
        &self.deny_env
    }

    /// The private git directory the host must authorize as a sandbox write
    /// root for the confined command.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Whether this invocation staged an approved credential value inside
    /// [`Self::root`]. A detached process outlives the guard that reaps the
    /// directory, so a caller must not hand a credential-bearing isolation to
    /// one: the value's lifetime could no longer be bounded to the execution.
    pub fn carries_credential(&self) -> bool {
        self.credential
    }

    /// Let a detached process outlive this value: the private directory stays
    /// until the operating system clears its temp area.
    ///
    /// A caller must check [`Self::carries_credential`] first: handing the
    /// staged credential to a detached process leaks it past every lifetime the
    /// runtime can bound.
    pub fn keep(mut self) -> Self {
        self.cleanup = false;
        self
    }
}

/// Build the frozen invocation for an approved remote Git command, or refuse.
///
/// Refusing is the correct outcome whenever the approved target cannot be
/// reproduced from frozen bytes — the caller then fails the command rather than
/// letting it run against live, mutable configuration.
#[cfg(unix)]
pub async fn isolate_git_command(
    target: &ApprovedGitTarget,
    args: &[String],
    cwd: &Path,
    environment: &EnvSnapshot,
) -> Result<GitIsolation, String> {
    let Some(operation) = args
        .first()
        .and_then(|subcommand| git_remote_operation("git", std::slice::from_ref(subcommand)))
    else {
        return Err("command is not a frozen Git target".into());
    };
    if operation.write() != target.write {
        return Err("approved Git target and command disagree on effect".into());
    }

    // Re-resolve the approved credential and prove it is still the same
    // incarnation before injecting it. A rotated token (or a switched account)
    // fails closed here so the old grant cannot authenticate as the new secret.
    let credential = match &target.credential {
        Some(approved) => {
            let material = crate::credential::fill_http_credential(
                &approved.host,
                &approved.transport,
                environment,
            )
            .await
            .map_err(|_| "approved credential source failed".to_string())?
            .ok_or_else(|| "approved credential is no longer available".to_string())?;
            if material.identity() != approved.identity {
                return Err("credential identity changed; resource binding refused".into());
            }
            Some(material)
        }
        None => None,
    };

    let canonical_cwd = std::fs::canonicalize(cwd)
        .map_err(|_| "cannot canonicalize Git command directory".to_string())?;
    let identity = crate::resource_identity::resolve_project_identity_with_environment(
        &canonical_cwd,
        environment,
    )
    .await?;
    if identity != target.repository {
        return Err("repository identity changed; resource binding refused".into());
    }

    let git_dir = std::fs::canonicalize(
        crate::resource_identity::git_read(
            &canonical_cwd,
            &["rev-parse", "--absolute-git-dir"],
            environment,
        )
        .await?
        .trim(),
    )
    .map_err(|_| "cannot canonicalize repository git directory".to_string())?;
    let common = crate::resource_identity::git_read(
        &canonical_cwd,
        &["rev-parse", "--git-common-dir"],
        environment,
    )
    .await?;
    let common = PathBuf::from(common.trim());
    let common = std::fs::canonicalize(if common.is_absolute() {
        common
    } else {
        canonical_cwd.join(common)
    })
    .map_err(|_| "cannot canonicalize repository common directory".to_string())?;
    // Shapes whose configuration or refs live somewhere the mirror cannot pin.
    if common != git_dir
        || git_dir.join("commondir").exists()
        || git_dir.join("config.worktree").exists()
        || git_dir.join("shallow").exists()
    {
        return Err("repository shape cannot be a frozen Git target".into());
    }
    let refs = git_dir.join("refs");
    if !refs.is_dir() {
        return Err("repository has no refs directory to freeze".into());
    }

    // Freeze the exact bytes that were verified: read once, verify those bytes,
    // then use those same bytes. Nothing re-reads the file afterwards.
    let config_bytes = match std::fs::read(git_dir.join("config")) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Vec::new(),
        Err(_) => return Err("cannot read repository config".into()),
    };

    let root = private_dir(&git_dir)?;
    let result = build_frozen_invocation(
        &root,
        &git_dir,
        &refs,
        &config_bytes,
        target,
        operation,
        args,
        environment,
        credential.as_ref(),
    )
    .await;
    match result {
        Ok((program, args, authority_env, deny_env)) => Ok(GitIsolation {
            program,
            args,
            authority_env,
            deny_env,
            root,
            cleanup: true,
            credential: credential.is_some(),
        }),
        Err(error) => {
            let _ = std::fs::remove_dir_all(&root);
            Err(error)
        }
    }
}

/// Frozen Git execution cannot be proven off unix: the mirror relies on
/// directory symlinks and there is no equivalent verified git-dir boundary.
#[cfg(not(unix))]
pub async fn isolate_git_command(
    _target: &ApprovedGitTarget,
    _args: &[String],
    _cwd: &Path,
    _environment: &EnvSnapshot,
) -> Result<GitIsolation, String> {
    Err("frozen Git execution is unsupported on this platform".into())
}

#[cfg(unix)]
#[allow(clippy::too_many_arguments)]
async fn build_frozen_invocation(
    root: &Path,
    git_dir: &Path,
    refs: &Path,
    config_bytes: &[u8],
    target: &ApprovedGitTarget,
    operation: GitRemoteOperation,
    args: &[String],
    environment: &EnvSnapshot,
    credential: Option<&crate::credential::CredentialMaterial>,
) -> Result<(String, Vec<String>, Vec<(String, String)>, Vec<String>), String> {
    use std::os::unix::fs::symlink;

    let config_path = root.join("config");
    std::fs::write(&config_path, config_bytes)
        .map_err(|_| "cannot stage frozen config".to_string())?;
    let config_path = config_path
        .to_str()
        .ok_or_else(|| "non-UTF8 isolation path".to_string())?
        .to_string();

    // Parse the bytes we just wrote with Git itself, so the frozen snapshot and
    // the verified snapshot are the same file.
    let raw = crate::resource_identity::git_read(
        root,
        &[
            "config",
            "--file",
            &config_path,
            "--no-includes",
            "--null",
            "--list",
        ],
        environment,
    )
    .await?;
    let config: Vec<(&str, &str)> = raw
        .split('\0')
        .filter(|entry| !entry.is_empty())
        .map(|entry| entry.split_once('\n').unwrap_or((entry, "")))
        .collect();
    if config
        .iter()
        .any(|(key, _)| effect_critical_config_key(key, false))
    {
        return Err(
            "repository config can change a remote effect; resource binding refused".into(),
        );
    }

    let regular = format!("remote.{}.url", target.remote_name);
    let push = format!("remote.{}.pushurl", target.remote_name);
    let mut urls = config
        .iter()
        .filter(|(key, _)| *key == regular)
        .map(|(_, value)| *value)
        .collect::<Vec<_>>();
    if operation.write() {
        let push_urls = config
            .iter()
            .filter(|(key, _)| *key == push)
            .map(|(_, value)| *value)
            .collect::<Vec<_>>();
        if !push_urls.is_empty() {
            urls = push_urls;
        }
    }
    if urls.len() != 1 {
        return Err("approved remote is not a single frozen URL".into());
    }
    let Some((canonical_url, transport)) = crate::resource_identity::canonical_remote_url(urls[0])
    else {
        return Err("approved remote URL is not a canonical HTTP(S) destination".into());
    };
    if canonical_url != target.canonical_url || transport != target.transport {
        return Err("configured remote changed; resource binding refused".into());
    }

    let hooks = root.join("hooks");
    let home = root.join("home");
    let xdg = root.join("xdg");
    for directory in [&hooks, &home, &xdg] {
        std::fs::create_dir(directory)
            .map_err(|_| "cannot stage Git isolation directory".to_string())?;
    }
    let global = root.join("global");
    let system = root.join("system");
    for file in [&global, &system] {
        std::fs::write(file, b"").map_err(|_| "cannot stage empty Git config".to_string())?;
    }

    // Stage the approved credential in the private directory and point Git at a
    // helper that prints it. The value goes into a 0600 file read by the helper
    // process, never onto the command line or into the child environment, so it
    // cannot appear in `ps`, an error message or a tool result.
    let credential_helper = match credential {
        Some(material) => {
            let file = root.join("credential");
            std::fs::write(&file, material.helper_payload())
                .map_err(|_| "cannot stage approved credential".to_string())?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o600))
                    .map_err(|_| "cannot protect approved credential".to_string())?;
            }
            let script = root.join("credential-helper");
            std::fs::write(
                &script,
                b"#!/bin/sh\n[ \"$1\" = \"get\" ] || exit 0\nexec cat \"$LEVELER_CREDENTIAL_FILE\"\n",
            )
            .map_err(|_| "cannot stage credential helper".to_string())?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o700))
                    .map_err(|_| "cannot protect credential helper".to_string())?;
            }
            Some((path_string(&script)?, path_string(&file)?))
        }
        None => None,
    };

    symlink(git_dir.join("objects"), root.join("objects"))
        .map_err(|_| "cannot mirror Git objects".to_string())?;
    symlink(refs, root.join("refs")).map_err(|_| "cannot mirror Git refs".to_string())?;
    std::fs::copy(git_dir.join("HEAD"), root.join("HEAD"))
        .map_err(|_| "cannot mirror Git HEAD".to_string())?;
    for name in ["packed-refs", "logs", "info"] {
        let source = git_dir.join(name);
        if source.exists() {
            symlink(&source, root.join(name))
                .map_err(|_| "cannot mirror Git metadata".to_string())?;
        }
    }

    let mut frozen = vec![
        "--git-dir".to_string(),
        root.to_str()
            .ok_or_else(|| "non-UTF8 isolation path".to_string())?
            .to_string(),
        "-c".to_string(),
        format!("core.hooksPath={}", hooks.display()),
        // Reset, then set: a helper from the staged repository config can never
        // run. The approved one, when present, is the only active helper.
        "-c".to_string(),
        "credential.helper=".to_string(),
        "-c".to_string(),
        format!("remote.{}.url={canonical_url}", target.remote_name),
    ];
    if let Some((script, _)) = &credential_helper {
        frozen.push("-c".to_string());
        frozen.push(format!("credential.helper=!{script}"));
    }
    if operation.write() {
        frozen.push("-c".to_string());
        frozen.push(format!(
            "remote.{}.pushurl={canonical_url}",
            target.remote_name
        ));
    }
    frozen.extend(args.iter().cloned());

    let mut authority_env = vec![
        ("GIT_CONFIG_NOSYSTEM".to_string(), "1".to_string()),
        ("GIT_CONFIG_GLOBAL".to_string(), path_string(&global)?),
        ("GIT_CONFIG_SYSTEM".to_string(), path_string(&system)?),
        ("HOME".to_string(), path_string(&home)?),
        ("XDG_CONFIG_HOME".to_string(), path_string(&xdg)?),
        ("GIT_TERMINAL_PROMPT".to_string(), "0".to_string()),
    ];
    if let Some((_, file)) = credential_helper {
        authority_env.push(("LEVELER_CREDENTIAL_FILE".to_string(), file));
    }
    let deny_env = [
        "GIT_DIR",
        "GIT_WORK_TREE",
        "GIT_COMMON_DIR",
        "GIT_INDEX_FILE",
        "GIT_OBJECT_DIRECTORY",
        "GIT_ALTERNATE_OBJECT_DIRECTORIES",
        "GIT_CONFIG",
        "GIT_CONFIG_COUNT",
        "GIT_CONFIG_PARAMETERS",
        "GIT_CONFIG_GLOBAL",
        "GIT_CONFIG_SYSTEM",
        "GIT_CONFIG_NOSYSTEM",
        "GIT_SSH",
        "GIT_SSH_COMMAND",
        "GIT_SSH_VARIANT",
        "GIT_ASKPASS",
        "GIT_EXTERNAL_DIFF",
        "GIT_PAGER",
    ]
    .into_iter()
    .map(str::to_string)
    .collect();

    Ok(("git".to_string(), frozen, authority_env, deny_env))
}

#[cfg(unix)]
fn private_dir(_git_dir: &Path) -> Result<PathBuf, String> {
    use std::os::unix::fs::PermissionsExt;
    // The host authorizes this one directory as an extra sandbox write root for
    // the command (see `ProcessRequest::sandbox_write_roots`), so it may live in
    // the shared temp tree without widening the workspace's write fence.
    let path =
        std::env::temp_dir().join(format!("leveler-git-{}", leveler_core::new_uuid_string()));
    std::fs::create_dir(&path).map_err(|_| "cannot create Git isolation directory".to_string())?;
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700))
        .map_err(|_| "cannot restrict Git isolation directory".to_string())?;
    Ok(path)
}

#[cfg(unix)]
fn path_string(path: &Path) -> Result<String, String> {
    path.to_str()
        .map(str::to_string)
        .ok_or_else(|| "non-UTF8 isolation path".to_string())
}
