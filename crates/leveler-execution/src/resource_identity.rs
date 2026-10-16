//! Host-resolved reusable identities. Unsupported attribution earns no grant.
use leveler_core::{Capability, EnvSnapshot, GrantBinding, GrantRequest, ResourceIdentity};
use std::path::{Path, PathBuf};

/// Bind the canonical checkout and Git common directory's current incarnation.
/// Non-repository workspaces use their canonical directory object identity.
pub async fn resolve_project_identity(cwd: &Path) -> Result<String, String> {
    resolve_project_identity_with_environment(cwd, leveler_core::environment()).await
}

/// Resolve under the same immutable environment as command execution.
pub async fn resolve_project_identity_with_environment(
    cwd: &Path,
    environment: &EnvSnapshot,
) -> Result<String, String> {
    if inherited_git_overrides(environment) {
        return Err("project identity cannot be attributed under inherited Git overrides".into());
    }
    if environment.var_os_case_insensitive("PATH")
        != leveler_core::environment().var_os_case_insensitive("PATH")
    {
        return Err(
            "project identity cannot be attributed under an alternate Git executable lookup".into(),
        );
    }
    if environment
        .paths_case_insensitive("PATH")
        .iter()
        .any(|directory| !directory.is_absolute())
    {
        return Err("relative executable lookup cannot prove Git project identity".into());
    }
    let canonical = std::fs::canonicalize(cwd)
        .map_err(|_| "cannot canonicalize project directory".to_string())?;
    let repository = canonical
        .ancestors()
        .any(|parent| parent.join(".git").exists());
    if !repository {
        return identity(&canonical, &canonical);
    }
    let checkout = canonical
        .ancestors()
        .find(|parent| parent.join(".git").exists())
        .ok_or("cannot locate repository directory")?;
    if environment
        .paths_case_insensitive("PATH")
        .iter()
        .any(|directory| {
            directory.starts_with(checkout)
                || std::fs::canonicalize(directory)
                    .is_ok_and(|directory| directory.starts_with(checkout))
        })
    {
        return Err("workspace executable lookup cannot prove Git project identity".into());
    }
    let root = git_read(&canonical, &["rev-parse", "--show-toplevel"], environment).await?;
    let common = git_read(&canonical, &["rev-parse", "--git-common-dir"], environment).await?;
    let root = std::fs::canonicalize(Path::new(root.trim()))
        .map_err(|_| "cannot canonicalize repository root".to_string())?;
    let common = PathBuf::from(common.trim());
    let common = std::fs::canonicalize(if common.is_absolute() {
        common
    } else {
        canonical.join(common)
    })
    .map_err(|_| "cannot canonicalize repository common directory".to_string())?;
    identity(&root, &common)
}

fn identity(root: &Path, object: &Path) -> Result<String, String> {
    use sha2::{Digest, Sha256};
    let payload = serde_json::to_vec(&(
        path_string(root)?,
        path_string(object)?,
        object_identity(object)?,
    ))
    .map_err(|_| "cannot encode resource identity".to_string())?;
    Ok(format!("sha256:{:x}", Sha256::digest(payload)))
}
fn path_string(path: &Path) -> Result<String, String> {
    path.to_str()
        .map(str::to_string)
        .ok_or_else(|| "non-UTF8 resource paths are unsupported".into())
}
fn object_identity(path: &Path) -> Result<String, String> {
    let metadata =
        std::fs::metadata(path).map_err(|_| "cannot read resource object identity".to_string())?;
    metadata_object_identity(&metadata)
}

fn metadata_object_identity(metadata: &std::fs::Metadata) -> Result<String, String> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        // Birth time, when provided by the filesystem, also resists inode reuse.
        let birth = metadata
            .created()
            .ok()
            .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|time| time.as_nanos());
        Ok(format!(
            "unix:{}:{}:{birth:?}",
            metadata.dev(),
            metadata.ino()
        ))
    }
    #[cfg(not(unix))]
    {
        let _ = metadata;
        Err("resource object incarnation proof unsupported on this platform".into())
    }
}
fn inherited_git_overrides(environment: &EnvSnapshot) -> bool {
    environment.vars_os().any(|(key, _)| {
        key.to_str().is_some_and(|key| {
            let key = key.to_ascii_uppercase();
            key.starts_with("GIT_")
                && !matches!(
                    key.as_str(),
                    "GIT_TERMINAL_PROMPT" | "GIT_OPTIONAL_LOCKS" | "GIT_PAGER"
                )
        })
    })
}

pub(crate) async fn git_read(
    cwd: &Path,
    args: &[&str],
    environment: &EnvSnapshot,
) -> Result<String, String> {
    let mut command = tokio::process::Command::new("git");
    command
        .current_dir(cwd)
        .args(args)
        .kill_on_drop(true)
        .env_clear()
        .envs(environment.scrubbed_vars_os());
    let output = tokio::time::timeout(std::time::Duration::from_secs(5), command.output())
        .await
        .map_err(|_| "Git resource resolution timed out".to_string())?
        .map_err(|_| "cannot run Git resource query".to_string())?;
    if !output.status.success() {
        return Err("Git resource query failed".into());
    }
    // Never return Git stderr: credential/config contents may contain secrets.
    String::from_utf8(output.stdout).map_err(|_| "Git resource query returned non-UTF8 data".into())
}

/// Resolve reusable grants for Git commands whose remote can be named.
/// A non-Git sibling or a shell redirect does not drop the binding. An
/// environment prefix that rebinds the repository, or a remote we cannot
/// name, keeps exact-call approval.
pub async fn resolve_git_grant(
    program: &str,
    args: &[String],
    cwd: &Path,
) -> Result<Option<GrantRequest>, String> {
    resolve_git_grant_with_environment(program, args, cwd, leveler_core::environment()).await
}

/// Use the command runner's immutable environment, never the live host env.
pub async fn resolve_git_grant_with_environment(
    program: &str,
    args: &[String],
    cwd: &Path,
    environment: &EnvSnapshot,
) -> Result<Option<GrantRequest>, String> {
    // Shell redirects must not inherit Git's metadata scope; that is an
    // effect decision. They also must not hide a resolvable remote from the
    // credential broker. An authority-changing environment prefix
    // (`GIT_DIR`, `GIT_SSH_COMMAND`) makes the resolved repository the wrong
    // one, so it keeps exact-call approval.
    let commands = crate::executed_commands(program, args);
    // A parsed list that is not complete is gating evidence only — the
    // granted argv must never be derived from it (`ExecutedCommands::complete`).
    if commands.commands.is_empty() || commands.env_prefixed || !commands.complete {
        return Ok(None);
    }
    let mut selections = Vec::new();
    let mut explicit_urls = Vec::new();
    let mut saw_remote = false;
    for command in &commands.commands {
        let Some((name, rest)) = command.split_first() else {
            continue;
        };
        if name != "git" {
            // A sibling must not ride the Git grant: reuse is offered only
            // when every sibling is provably harmless. Anything the
            // classifier cannot call Safe keeps exact-call approval.
            if crate::approval::classify_command(&crate::approval::CommandView {
                program: name,
                args: rest,
            }) == crate::approval::CommandClass::Dangerous
            {
                return Ok(None);
            }
            continue;
        }
        let remote = crate::git_effects::git_command_effects(command)
            .is_some_and(|effects| effects.effects.remote_read || effects.effects.remote_write);
        if let Some(selection) = crate::git_effects::git_grant_selection(command) {
            if selection.remote_name.is_some() {
                saw_remote = true;
            }
            selections.push(selection);
        } else if let Some(url) = crate::git_effects::single_explicit_remote_target(command) {
            saw_remote = true;
            explicit_urls.push(url);
        } else if remote {
            return Ok(None);
        }
    }
    if !saw_remote {
        return Ok(None);
    }
    // Inherited process overrides must not silently change Git's config or root.
    if inherited_git_overrides(environment) {
        return Ok(None);
    }
    // A resource grant cannot treat a caller-selected PATH executable as Git.
    // Preserve exact-call approval when execution changes the host tool lookup.
    if environment.var_os_case_insensitive("PATH")
        != leveler_core::environment().var_os_case_insensitive("PATH")
    {
        return Ok(None);
    }
    let canonical = std::fs::canonicalize(cwd)
        .map_err(|_| "cannot canonicalize command directory".to_string())?;
    let checkout = canonical
        .ancestors()
        .find(|parent| parent.join(".git").exists())
        .unwrap_or(&canonical);
    if environment
        .paths_case_insensitive("PATH")
        .iter()
        .any(|directory| {
            !directory.is_absolute()
                || directory.starts_with(checkout)
                || std::fs::canonicalize(directory)
                    .is_ok_and(|directory| directory.starts_with(checkout))
        })
    {
        return Ok(None);
    }
    let project_identity = resolve_project_identity_with_environment(cwd, environment).await?;
    let raw = git_read(cwd, &["config", "--null", "--list"], environment).await?;
    let local_raw = git_read(cwd, &["config", "--local", "--null", "--list"], environment).await?;
    let config: Vec<(&str, &str)> = raw
        .split('\0')
        .filter(|entry| !entry.is_empty())
        .map(|entry| entry.split_once('\n').unwrap_or((entry, "")))
        .collect();
    let local_config: Vec<(&str, &str)> = local_raw
        .split('\0')
        .filter(|entry| !entry.is_empty())
        .map(|entry| entry.split_once('\n').unwrap_or((entry, "")))
        .collect();
    let workspace_mutation = selections
        .iter()
        .any(|selection| selection.effects.workspace_mutation);
    for (key, _) in &config {
        if crate::git_isolation::effect_critical_config_key(key, workspace_mutation) {
            return Ok(None);
        }
    }
    if crate::git_isolation::has_active_credential_helper(&local_config) {
        // A repository-local helper is a program the checkout itself supplies.
        // It cannot be attributed or brokered, so it keeps exact-call approval
        // rather than silently becoming the credential source of a grant.
        return Ok(None);
    }
    let hooks = git_read(cwd, &["rev-parse", "--git-path", "hooks"], environment).await?;
    let hooks = PathBuf::from(hooks.trim());
    let hooks = if hooks.is_absolute() {
        hooks
    } else {
        cwd.join(hooks)
    };
    if hooks.exists() {
        for entry in
            std::fs::read_dir(hooks).map_err(|_| "cannot inspect repository hooks".to_string())?
        {
            let entry = entry.map_err(|_| "cannot inspect repository hook".to_string())?;
            if !entry.file_name().to_string_lossy().ends_with(".sample") && entry.path().is_file() {
                let metadata = entry
                    .metadata()
                    .map_err(|_| "cannot inspect repository hook permissions".to_string())?;
                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt;
                    if metadata.permissions().mode() & 0o111 != 0 {
                        return Ok(None);
                    }
                }
                #[cfg(not(unix))]
                {
                    let _ = metadata;
                    return Ok(None);
                }
            }
        }
    }
    let mut bindings = Vec::new();
    for selection in selections {
        let effects = selection.effects;
        let repo = ResourceIdentity::Repository {
            identity: project_identity.clone(),
        };
        for (needed, capability) in [
            (effects.metadata_read, Capability::RepositoryRead),
            (effects.metadata_write, Capability::RepositoryMetadataWrite),
            (effects.workspace_mutation, Capability::RepositoryMutate),
            (effects.config_write, Capability::RepositoryConfigWrite),
            (
                effects.irreversible && !effects.remote_write,
                Capability::RepositoryDestroy,
            ),
        ] {
            if needed {
                push_binding(
                    &mut bindings,
                    GrantBinding {
                        capability,
                        resource: repo.clone(),
                    },
                );
            }
        }
        if let Some(name) = selection.remote_name {
            let regular = format!("remote.{name}.url");
            let push = format!("remote.{name}.pushurl");
            let mut urls = config
                .iter()
                .filter(|(key, _)| *key == regular)
                .map(|(_, value)| *value)
                .collect::<Vec<_>>();
            if effects.remote_write {
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
                return Ok(None);
            }
            let Some((canonical_url, transport)) = canonical_remote_url(urls[0]) else {
                return Ok(None);
            };
            let host = url::Url::parse(&canonical_url)
                .ok()
                .and_then(|url| url.host_str().map(str::to_string));
            let resource = ResourceIdentity::ConfiguredRemote {
                repository: project_identity.clone(),
                remote_name: name,
                canonical_url: canonical_url.clone(),
                transport: transport.clone(),
            };
            push_binding(
                &mut bindings,
                GrantBinding {
                    capability: if effects.remote_write {
                        if effects.irreversible {
                            Capability::RemoteForce
                        } else {
                            Capability::RemoteMutate
                        }
                    } else {
                        Capability::RemoteRead
                    },
                    resource,
                },
            );
            // Using the destination's credential is its own capability. It is
            // bound to the credential's exact incarnation, so rotating the
            // secret (or switching account) invalidates the old grant. A public
            // destination has no stored credential and gets no binding, so it
            // never prompts.
            if let Some(host) = host
                && let Ok(Some(material)) =
                    crate::credential::fill_http_credential(&host, &transport, environment).await
            {
                push_binding(
                    &mut bindings,
                    GrantBinding {
                        capability: Capability::CredentialUse,
                        resource: ResourceIdentity::Credential {
                            project: project_identity.clone(),
                            host,
                            identity: material.identity(),
                            transport,
                        },
                    },
                );
            }
        }
    }
    // An explicit https URL is not a configured remote, so it has no
    // ConfiguredRemote binding. When that host has a stored credential, the
    // use still has to be approved. A public URL adds nothing and does not ask.
    for raw in explicit_urls {
        let Some((canonical_url, transport)) = canonical_remote_url(&raw) else {
            continue;
        };
        let Some(host) = url::Url::parse(&canonical_url)
            .ok()
            .and_then(|url| url.host_str().map(str::to_string))
        else {
            continue;
        };
        if let Ok(Some(material)) =
            crate::credential::fill_http_credential(&host, &transport, environment).await
        {
            push_binding(
                &mut bindings,
                GrantBinding {
                    capability: Capability::CredentialUse,
                    resource: ResourceIdentity::Credential {
                        project: project_identity.clone(),
                        host,
                        identity: material.identity(),
                        transport,
                    },
                },
            );
        }
    }
    if bindings.is_empty() {
        return Ok(None);
    }
    Ok(Some(GrantRequest {
        project_identity,
        bindings,
    }))
}
fn push_binding(bindings: &mut Vec<GrantBinding>, binding: GrantBinding) {
    if !bindings.contains(&binding) {
        bindings.push(binding);
    }
}
pub(crate) fn canonical_remote_url(raw: &str) -> Option<(String, String)> {
    // SSH host aliases/ProxyCommand and keys need the credential host's proof.
    // A URL alone cannot bind their actual destination or executable effects.
    let url = url::Url::parse(raw).ok()?;
    if !matches!(url.scheme(), "https" | "http")
        || url.host_str().is_none()
        || url.password().is_some()
        || !url.username().is_empty()
        || url.query().is_some()
        || url.fragment().is_some()
    {
        return None;
    }
    Some((url.to_string(), url.scheme().to_string()))
}

/// Construct a descriptor-bound receipt without reading the path again.
/// The caller supplies its authoritative canonical destination and the metadata
/// obtained from the held file or parent descriptor before publishing/removing.
pub fn filesystem_resource_from_metadata(
    canonical_path: &Path,
    metadata: &std::fs::Metadata,
) -> Result<ResourceIdentity, String> {
    Ok(ResourceIdentity::FilesystemPath {
        canonical_path: path_string(canonical_path)?,
        object_identity: metadata_object_identity(metadata)?,
    })
}

/// New targets bind the nearest existing parent, plus their exact canonical path.
pub fn resolve_filesystem_resource(path: &Path) -> Result<ResourceIdentity, String> {
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .map_err(|_| "cannot read current directory".to_string())?
            .join(path)
    };
    let mut current = absolute.as_path();
    let mut missing = Vec::new();
    while !current.exists() {
        if std::fs::symlink_metadata(current).is_ok() {
            return Err("dangling filesystem symlink is not a reusable resource".into());
        }
        let name = current
            .file_name()
            .ok_or("cannot resolve filesystem target")?;
        missing.push(name.to_os_string());
        current = current.parent().ok_or("cannot resolve filesystem parent")?;
    }
    let mut canonical = std::fs::canonicalize(current)
        .map_err(|_| "cannot canonicalize filesystem resource".to_string())?;
    let object = object_identity(&canonical)?;
    for name in missing.into_iter().rev() {
        canonical.push(name);
    }
    Ok(ResourceIdentity::FilesystemPath {
        canonical_path: path_string(&canonical)?,
        object_identity: object,
    })
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    fn git(dir: &Path, args: &[&str]) {
        let output = std::process::Command::new("git")
            .current_dir(dir)
            .env_remove("GIT_DIR")
            .env_remove("GIT_WORK_TREE")
            .args(args)
            .output()
            .unwrap();
        assert!(output.status.success(), "git fixture setup failed");
    }
    fn repo() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        git(dir.path(), &["init", "-q"]);
        // Reset inherited helpers so the fixture has no extra executable authority.
        git(dir.path(), &["config", "credential.helper", ""]);
        git(
            dir.path(),
            &["remote", "add", "origin", "https://example.com/one.git"],
        );
        dir
    }
    async fn request(dir: &Path, args: &[&str]) -> Option<GrantRequest> {
        resolve_git_grant(
            "git",
            &args.iter().map(|arg| arg.to_string()).collect::<Vec<_>>(),
            dir,
        )
        .await
        .unwrap()
    }
    #[tokio::test]
    async fn live_git_resources_change_on_url_mutation_and_repository_replacement() {
        let dir = repo();
        let first = request(dir.path(), &["fetch", "origin"]).await.unwrap();
        git(
            dir.path(),
            &["remote", "set-url", "origin", "https://example.com/two.git"],
        );
        let changed = request(dir.path(), &["fetch", "origin"]).await.unwrap();
        assert_eq!(first.project_identity, changed.project_identity);
        assert_ne!(first.bindings, changed.bindings);
        std::fs::rename(dir.path().join(".git"), dir.path().join("old-git")).unwrap();
        git(dir.path(), &["init", "-q"]);
        git(dir.path(), &["config", "credential.helper", ""]);
        git(
            dir.path(),
            &["remote", "add", "origin", "https://example.com/two.git"],
        );
        let replaced = request(dir.path(), &["fetch", "origin"]).await.unwrap();
        assert_ne!(changed.project_identity, replaced.project_identity);
    }
    #[tokio::test]
    async fn capabilities_and_unresolved_calls_do_not_alias() {
        let dir = repo();
        let read = request(dir.path(), &["fetch", "origin"]).await.unwrap();
        let push = request(dir.path(), &["push", "origin", "main"])
            .await
            .unwrap();
        let force = request(dir.path(), &["push", "--force", "origin", "main"])
            .await
            .unwrap();
        assert_ne!(read.bindings, push.bindings);
        assert_ne!(push.bindings, force.bindings);
        for args in [
            vec!["fetch", "https://example.com/two.git"],
            vec!["-c", "core.hooksPath=/tmp/evil", "fetch", "origin"],
            vec!["fetch", "--all"],
            vec!["fetch", "missing"],
        ] {
            assert!(request(dir.path(), &args).await.is_none());
        }
        let mixed = resolve_git_grant(
            "sh",
            &["-c".into(), "git fetch origin; echo extra".into()],
            dir.path(),
        )
        .await
        .unwrap();
        assert_eq!(mixed, Some(read.clone()));
        let wrapped_push = resolve_git_grant(
            "sh",
            &["-c".into(), "git push origin main".into()],
            dir.path(),
        )
        .await
        .unwrap();
        assert_eq!(wrapped_push, Some(push.clone()));
        let wrapped =
            resolve_git_grant("sh", &["-c".into(), "git fetch origin".into()], dir.path())
                .await
                .unwrap();
        assert_eq!(wrapped, Some(read.clone()));
        assert!(
            resolve_git_grant("sh", &["-c".into(), "git fetch $REMOTE".into()], dir.path())
                .await
                .unwrap()
                .is_none()
        );
        git(
            dir.path(),
            &["remote", "set-url", "origin", "ssh://git@example.com/repo"],
        );
        assert!(request(dir.path(), &["fetch", "origin"]).await.is_none());
        git(
            dir.path(),
            &["remote", "set-url", "origin", "https://example.com/one.git"],
        );
        git(
            dir.path(),
            &[
                "config",
                "credential.https://example.com.helper",
                "!fixture-helper",
            ],
        );
        assert!(request(dir.path(), &["fetch", "origin"]).await.is_none());
        git(
            dir.path(),
            &["config", "--unset", "credential.https://example.com.helper"],
        );
        git(
            dir.path(),
            &[
                "config",
                "credential.https://example.com/A.helper",
                "!case-sensitive-helper",
            ],
        );
        git(
            dir.path(),
            &["config", "credential.https://example.com/a.helper", ""],
        );
        assert!(
            request(dir.path(), &["fetch", "origin"]).await.is_none(),
            "different URL path scopes must not clear each other's helper authority"
        );
        git(
            dir.path(),
            &[
                "config",
                "--unset",
                "credential.https://example.com/A.helper",
            ],
        );

        git(
            dir.path(),
            &[
                "config",
                "remote.origin.pushurl",
                "https://example.com/push.git",
            ],
        );
        let pushed = request(dir.path(), &["push", "origin", "main"])
            .await
            .unwrap();
        assert!(pushed.bindings.iter().any(|binding| matches!(&binding.resource, ResourceIdentity::ConfiguredRemote { canonical_url, .. } if canonical_url.ends_with("/push.git"))));
        git(
            dir.path(),
            &[
                "config",
                "url.https://elsewhere.example/.insteadOf",
                "https://example.com/",
            ],
        );
        assert!(request(dir.path(), &["fetch", "origin"]).await.is_none());
    }
    #[test]
    fn filesystem_identity_invalidates_on_object_and_symlink_replacement() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("file");
        std::fs::write(&path, "old").unwrap();
        let original = resolve_filesystem_resource(&path).unwrap();
        let held_metadata = std::fs::File::open(&path).unwrap().metadata().unwrap();
        let canonical_path = std::fs::canonicalize(&path).unwrap();
        std::fs::rename(&path, dir.path().join("previous")).unwrap();
        std::fs::write(&path, "new").unwrap();
        assert_eq!(
            original,
            filesystem_resource_from_metadata(&canonical_path, &held_metadata).unwrap(),
            "receipt remains bound to the held object after path replacement"
        );
        assert_ne!(original, resolve_filesystem_resource(&path).unwrap());
        #[cfg(unix)]
        {
            let link = dir.path().join("link");
            std::os::unix::fs::symlink(&path, &link).unwrap();
            let before = resolve_filesystem_resource(&link).unwrap();
            std::fs::remove_file(&link).unwrap();
            std::os::unix::fs::symlink(dir.path().join("previous"), &link).unwrap();
            assert_ne!(before, resolve_filesystem_resource(&link).unwrap());
        }
        let missing = dir.path().join("new-file");
        let absent = resolve_filesystem_resource(&missing).unwrap();
        std::fs::write(&missing, "exists").unwrap();
        assert_ne!(absent, resolve_filesystem_resource(&missing).unwrap());
    }
    #[tokio::test]
    async fn project_identity_rejects_inherited_repository_overrides() {
        if std::env::var_os("LEVELER_TEST_PROJECT_OVERRIDE_CHILD").is_some() {
            let dir = repo();
            assert!(
                resolve_project_identity_with_environment(
                    dir.path(),
                    &EnvSnapshot::new(
                        std::env::vars_os(),
                        dir.path().to_path_buf(),
                        dir.path().to_path_buf()
                    )
                )
                .await
                .is_err(),
                "GIT_DIR/GIT_WORK_TREE must not silently change the project identity"
            );
            return;
        }
        let foreign = repo();
        for (name, value) in [
            ("GIT_DIR", foreign.path().join(".git")),
            ("GIT_WORK_TREE", foreign.path().to_path_buf()),
        ] {
            let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "resource_identity::tests::project_identity_rejects_inherited_repository_overrides",
            ])
            .env("LEVELER_TEST_PROJECT_OVERRIDE_CHILD", "1")
            .env(name, &value)
            .output()
            .unwrap();
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stdout)
            );
        }
    }
    #[tokio::test]
    async fn alternate_snapshot_git_lookup_and_config_cannot_earn_a_resource_grant() {
        let dir = repo();
        let base = leveler_core::environment();
        for (name, value) in [
            ("PATH", dir.path().join("bin").into_os_string()),
            (
                "GIT_CONFIG_GLOBAL",
                dir.path().join("other-config").into_os_string(),
            ),
        ] {
            let mut values = base
                .vars_os()
                .map(|(name, value)| (name.clone(), value.clone()))
                .collect::<std::collections::BTreeMap<_, _>>();
            values.insert(name.into(), value);
            let snapshot =
                EnvSnapshot::new(values, dir.path().to_path_buf(), dir.path().to_path_buf());
            assert!(
                resolve_git_grant_with_environment(
                    "git",
                    &["fetch".into(), "origin".into()],
                    dir.path(),
                    &snapshot
                )
                .await
                .unwrap()
                .is_none()
            );
            assert!(
                resolve_project_identity_with_environment(dir.path(), &snapshot)
                    .await
                    .is_err()
            );
        }
    }
    #[tokio::test]
    async fn executable_hooks_cannot_inherit_remote_grants() {
        use std::os::unix::fs::PermissionsExt;
        let dir = repo();
        let hook = dir.path().join(".git/hooks/pre-push");
        std::fs::write(&hook, "#!/bin/sh\nexit 0\n").unwrap();
        std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert!(
            request(dir.path(), &["push", "origin", "main"])
                .await
                .is_some(),
            "non-executable hook does not run"
        );
        std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert!(
            request(dir.path(), &["push", "origin", "main"])
                .await
                .is_none(),
            "active hook has unbound executable effects"
        );
    }

    #[tokio::test]
    async fn a_dangerous_sibling_keeps_exact_call_approval() {
        let dir = repo();
        // The wrapper support that lets a harmless sibling reuse a grant
        // stays: a provably harmless sibling does not disable reuse.
        let harmless = resolve_git_grant(
            "sh",
            &["-c".into(), "git fetch origin; echo extra".into()],
            dir.path(),
        )
        .await
        .unwrap();
        assert!(harmless.is_some(), "harmless sibling keeps reuse");

        // A sibling the classifier cannot call Safe must not ride the Git
        // grant: the user approved the Git call, not the whole script.
        for script in [
            "git fetch origin; rm -rf /abs/path",
            "git push origin main; rm -rf /abs/path",
            "git fetch origin; rm -rf \"$TARGET\"",
            "git fetch origin; sudo rm -rf /x",
            "git fetch origin && open /tmp/file",
        ] {
            let got = resolve_git_grant("sh", &["-c".into(), script.to_string()], dir.path())
                .await
                .unwrap();
            assert!(
                got.is_none(),
                "a dangerous sibling must keep exact-call approval: {script}"
            );
        }
    }
}
