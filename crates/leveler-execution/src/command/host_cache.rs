//! Host-side cache and scratch preparation for sandboxed commands.
//!
//! A confined command may not write to the host's toolchain caches, but
//! rebuilding every dependency per command is prohibitively slow. This module
//! builds the private, workspace-external replacement: a per-command scratch
//! directory plus a per-workspace tool cache under Leveler's own home, with
//! read-only overlays onto the host caches where that is safe.
//!
//! The whole module is macOS/Linux-only; [`super`] gates it behind the same
//! `cfg`, so individual items carry no target attributes.
//!
//! Entry points: [`prepare_sandbox_paths`] builds the [`SandboxPaths`] handed
//! to a child, and [`apply_sandbox_environment`] redirects that child's
//! toolchain environment variables at spawn time.

use std::path::{Component, Path, PathBuf};

use sha2::{Digest, Sha256};
use tokio::process::Command;

/// Isolated writable paths for a confined command. Temporary files are unique
/// per command, while build caches persist per workspace under Leveler's own
/// home so common builds do not repeatedly download dependencies.
pub(crate) struct SandboxPaths {
    scratch: tempfile::TempDir,
    tool_cache: PathBuf,
    cargo_home: PathBuf,
    go_mod_cache: PathBuf,
    npm_cache: PathBuf,
    cache_write_roots: Vec<PathBuf>,
}

impl SandboxPaths {
    pub(crate) fn scratch_path(&self) -> &Path {
        self.scratch.path()
    }

    pub(crate) fn tool_cache_path(&self) -> &Path {
        &self.tool_cache
    }

    pub(crate) fn cache_write_roots(&self) -> &[PathBuf] {
        &self.cache_write_roots
    }

    pub(crate) fn into_scratch(self) -> tempfile::TempDir {
        self.scratch
    }
}

/// Open/create one real child directory relative to an already-open capability.
/// Poisoned links/files are removed through the parent handle without following
/// them. Returning the child handle closes the check/use race for later steps.
fn ensure_real_private_child(
    parent: &cap_std::fs::Dir,
    parent_path: &Path,
    name: &std::ffi::OsStr,
) -> std::io::Result<(cap_std::fs::Dir, PathBuf)> {
    // Another Leveler process may be initializing or repairing the same
    // per-workspace cache concurrently. Retry a bounded number of times while
    // always resolving the final entry without following links.
    for _ in 0..16 {
        match parent.symlink_metadata(name) {
            Ok(metadata) if metadata.is_dir() && !metadata.file_type().is_symlink() => {
                match parent.open_dir(name) {
                    Ok(child) => return Ok((child, parent_path.join(name))),
                    Err(error)
                        if matches!(
                            error.kind(),
                            std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory
                        ) =>
                    {
                        continue;
                    }
                    Err(error) => return Err(error),
                }
            }
            Ok(_) => {
                if let Err(error) = parent.remove_file(name) {
                    match parent.symlink_metadata(name) {
                        Ok(metadata) if metadata.is_dir() && !metadata.file_type().is_symlink() => {
                            continue;
                        }
                        Err(current) if current.kind() == std::io::ErrorKind::NotFound => {
                            continue;
                        }
                        _ if error.kind() == std::io::ErrorKind::NotFound => continue,
                        _ => return Err(error),
                    }
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                if let Err(error) = parent.create_dir(name)
                    && error.kind() != std::io::ErrorKind::AlreadyExists
                {
                    return Err(error);
                }
            }
            Err(error) => return Err(error),
        }
    }
    Err(std::io::Error::new(
        std::io::ErrorKind::WouldBlock,
        format!(
            "private cache entry {} changed repeatedly during initialization",
            parent_path.join(name).display()
        ),
    ))
}

fn ensure_real_private_chain(
    base: &cap_std::fs::Dir,
    base_path: &Path,
    relative: &Path,
) -> std::io::Result<PathBuf> {
    let mut current = base.try_clone()?;
    let mut current_path = base_path.to_path_buf();
    for component in relative.components() {
        let Component::Normal(name) = component else {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "private cache path must contain only normal components",
            ));
        };
        (current, current_path) = ensure_real_private_child(&current, &current_path, name)?;
    }
    Ok(current_path)
}

fn canonicalize_allow_missing(path: &Path) -> std::io::Result<PathBuf> {
    let mut existing = path;
    let mut suffix = Vec::new();
    while !existing.exists() {
        let Some(name) = existing.file_name() else {
            return Err(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                format!("no existing ancestor for {}", path.display()),
            ));
        };
        suffix.push(name.to_os_string());
        existing = existing.parent().ok_or_else(|| {
            std::io::Error::new(std::io::ErrorKind::NotFound, "cache path has no parent")
        })?;
    }
    let mut resolved = existing.canonicalize()?;
    for name in suffix.into_iter().rev() {
        resolved.push(name);
    }
    Ok(resolved)
}

fn open_cache_owner_outside_workspace(
    candidate: &Path,
    workspace: &Path,
) -> std::io::Result<(cap_std::fs::Dir, PathBuf)> {
    let resolved = canonicalize_allow_missing(candidate)?;
    if resolved.starts_with(workspace) {
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "tool-cache owner directory is inside the writable workspace",
        ));
    }
    if std::fs::symlink_metadata(candidate).is_ok_and(|metadata| metadata.file_type().is_symlink())
    {
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "tool-cache owner directory must not be a symlink",
        ));
    }
    std::fs::create_dir_all(candidate)?;
    let candidate = candidate.canonicalize()?;
    if candidate.starts_with(workspace) {
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "tool-cache owner resolved inside the writable workspace",
        ));
    }
    let dir = cap_std::fs::Dir::open_ambient_dir(&candidate, cap_std::ambient_authority())?;
    Ok((dir, candidate))
}

/// Open a private per-user cache owner under the captured host temp directory.
/// This is the fallback when no home directory capability was supplied. The
/// temp base itself must be a real, workspace-external directory; the child is
/// repaired relative to an open base handle and forced to mode 0700.
fn open_temp_cache_owner_outside_workspace(
    temp_base: &Path,
    workspace: &Path,
) -> std::io::Result<(cap_std::fs::Dir, PathBuf)> {
    if !temp_base.is_absolute()
        || temp_base.starts_with(workspace)
        || std::fs::symlink_metadata(temp_base)
            .is_ok_and(|metadata| metadata.file_type().is_symlink() || !metadata.is_dir())
    {
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "temporary cache base must be a real directory",
        ));
    }
    let temp_base = temp_base.canonicalize()?;
    if temp_base.starts_with(workspace) {
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "temporary cache base is inside the writable workspace",
        ));
    }
    let base = cap_std::fs::Dir::open_ambient_dir(&temp_base, cap_std::ambient_authority())?;
    let name = format!("codeleveler-private-{}", nix::unistd::geteuid().as_raw());
    let (owner, owner_path) = ensure_real_private_child(&base, &temp_base, name.as_ref())?;
    use cap_std::fs::PermissionsExt as _;
    owner.set_permissions(".", cap_std::fs::Permissions::from_mode(0o700))?;
    Ok((owner, owner_path))
}

pub(crate) fn prepare_sandbox_paths(
    environment: &leveler_core::EnvSnapshot,
    workspace: &Path,
    read_host_caches: bool,
) -> std::io::Result<SandboxPaths> {
    let workspace = workspace
        .canonicalize()
        .unwrap_or_else(|_| workspace.to_path_buf());
    let mut candidates = Vec::new();
    if let Some(leveler_home) = leveler_core::leveler_home_dir(environment) {
        candidates.push(leveler_home);
    }
    if let Some(home) = environment.var_os("HOME") {
        candidates.push(PathBuf::from(home).join(".cache/codeleveler-private"));
    }
    let (owner, owner_path) = candidates
        .iter()
        .find_map(|candidate| open_cache_owner_outside_workspace(candidate, &workspace).ok())
        .or_else(|| {
            open_temp_cache_owner_outside_workspace(environment.temp_dir(), &workspace).ok()
        })
        .ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                "no stable tool-cache owner outside the writable workspace",
            )
        })?;
    // Scratch creation uses the same stable, workspace-external owner rather
    // than TMPDIR, which may itself live in or be poisoned by the workspace.
    let scratch = tempfile::Builder::new()
        .prefix("codeleveler-sandbox-")
        .tempdir_in(&owner_path)?;
    let (cache_base_dir, cache_base) =
        ensure_real_private_child(&owner, &owner_path, "tool-cache".as_ref())?;
    let digest = Sha256::digest(workspace.as_os_str().as_encoded_bytes());
    let workspace_key = digest
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    let (tool_cache_dir, tool_cache) =
        ensure_real_private_child(&cache_base_dir, &cache_base, workspace_key.as_ref())?;

    #[cfg(unix)]
    for directory in [&cache_base, &tool_cache] {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(directory, std::fs::Permissions::from_mode(0o700))?;
    }

    std::fs::create_dir(scratch.path().join("tmp"))?;
    let mut cache_write_roots = Vec::new();
    for relative in [
        "cargo/registry",
        "cargo/git",
        "go/build",
        "go/mod",
        "go/path",
        "npm",
        "yarn",
        "pnpm",
        "pip",
        "uv",
        "xdg-cache",
    ] {
        cache_write_roots.push(ensure_real_private_chain(
            &tool_cache_dir,
            &tool_cache,
            Path::new(relative),
        )?);
    }
    let cargo_home = prepare_cargo_home(
        environment,
        &scratch,
        &cache_base,
        &tool_cache,
        &workspace,
        read_host_caches,
    )?;
    let go_mod_cache = if read_host_caches {
        host_go_mod_cache(environment, &workspace).unwrap_or_else(|| tool_cache.join("go/mod"))
    } else {
        tool_cache.join("go/mod")
    };
    let npm_cache = prepare_npm_cache(
        environment,
        &scratch,
        &tool_cache,
        &workspace,
        read_host_caches,
    )?;
    Ok(SandboxPaths {
        scratch,
        tool_cache,
        cargo_home,
        go_mod_cache,
        npm_cache,
        cache_write_roots,
    })
}

fn stable_host_directory(path: PathBuf, workspace: &Path) -> Option<PathBuf> {
    let entry = path.parent()?.canonicalize().ok()?.join(path.file_name()?);
    if entry.starts_with(workspace) {
        return None;
    }
    let metadata = std::fs::symlink_metadata(&path).ok()?;
    if !metadata.is_dir() || metadata.file_type().is_symlink() {
        return None;
    }
    path.canonicalize()
        .ok()
        .filter(|path| !path.starts_with(workspace))
}

fn host_cargo_home(
    environment: &leveler_core::EnvSnapshot,
    workspace: &Path,
    private_cache_base: Option<&Path>,
) -> Option<PathBuf> {
    environment
        .var_os("CARGO_HOME")
        .map(PathBuf::from)
        .or_else(|| {
            environment
                .var_os("HOME")
                .map(|home| PathBuf::from(home).join(".cargo"))
        })
        .and_then(|path| stable_host_directory(path, workspace))
        // A caller can configure CARGO_HOME arbitrarily. Never import config or
        // dependency sources from the same subtree that confined children can
        // write through Leveler's private cache mounts.
        .filter(|path| private_cache_base.is_none_or(|cache| !path.starts_with(cache)))
}

fn host_go_mod_cache(environment: &leveler_core::EnvSnapshot, workspace: &Path) -> Option<PathBuf> {
    environment
        .var_os("GOMODCACHE")
        .map(PathBuf::from)
        .or_else(|| {
            environment
                .paths("GOPATH")
                .into_iter()
                .next()
                .map(|path| path.join("pkg/mod"))
        })
        .or_else(|| {
            environment
                .var_os("HOME")
                .map(|home| PathBuf::from(home).join("go/pkg/mod"))
        })
        .and_then(|path| stable_host_directory(path, workspace))
}

fn host_npm_cache(environment: &leveler_core::EnvSnapshot, workspace: &Path) -> Option<PathBuf> {
    environment
        .var_os("npm_config_cache")
        .map(PathBuf::from)
        .or_else(|| {
            environment
                .var_os("HOME")
                .map(|home| PathBuf::from(home).join(".npm"))
        })
        .and_then(|path| stable_host_directory(path, workspace))
}

/// Replace one entry relative to a stable directory capability with a symlink.
/// Removal is no-follow and capability-relative, so a poisoned destination can
/// never redirect host initialization outside this directory.
fn replace_with_readonly_link(
    directory: &cap_std::fs::Dir,
    source: &Path,
    destination: &str,
) -> std::io::Result<()> {
    match directory.symlink_metadata(destination) {
        Ok(metadata) if metadata.is_dir() && !metadata.file_type().is_symlink() => {
            directory.remove_dir_all(destination)?;
        }
        Ok(_) => directory.remove_file(destination)?,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }
    directory.symlink_contents(source.canonicalize()?, destination)
}

fn sync_cargo_config(host: &Path, private: &cap_std::fs::Dir) -> std::io::Result<()> {
    const MAX_CARGO_CONFIG_BYTES: u64 = 1024 * 1024;
    let host = cap_std::fs::Dir::open_ambient_dir(host, cap_std::ambient_authority())?;
    for name in ["config", "config.toml", "credentials", "credentials.toml"] {
        use cap_std::fs::OpenOptionsExt as _;
        let mut options = cap_std::fs::OpenOptions::new();
        options
            .read(true)
            .custom_flags(nix::libc::O_NOFOLLOW | nix::libc::O_NONBLOCK);
        let source = match host.open_with(name, &options) {
            Ok(source) => Some(source),
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::NotFound | std::io::ErrorKind::PermissionDenied
                ) || error.raw_os_error() == Some(nix::libc::ELOOP) =>
            {
                None
            }
            Err(error) => return Err(error),
        };
        let mut copied = false;
        if let Some(source) = source {
            let metadata = source.metadata()?;
            if metadata.is_file() {
                if metadata.len() > MAX_CARGO_CONFIG_BYTES {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::InvalidData,
                        format!("Cargo {name} exceeds 1 MiB safety limit"),
                    ));
                }
                use std::io::Read as _;
                let mut bytes = Vec::new();
                source
                    .take(MAX_CARGO_CONFIG_BYTES + 1)
                    .read_to_end(&mut bytes)?;
                if bytes.len() as u64 > MAX_CARGO_CONFIG_BYTES {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::InvalidData,
                        format!("Cargo {name} exceeds 1 MiB safety limit"),
                    ));
                }
                private.write(name, bytes)?;
                copied = true;
            }
        }
        if !copied && let Ok(metadata) = private.symlink_metadata(name) {
            if metadata.is_dir() && !metadata.file_type().is_symlink() {
                private.remove_dir_all(name)?;
            } else {
                private.remove_file(name)?;
            }
        }
    }
    Ok(())
}

fn prepare_cargo_home(
    environment: &leveler_core::EnvSnapshot,
    scratch: &tempfile::TempDir,
    private_cache_base: &Path,
    tool_cache: &Path,
    workspace: &Path,
    read_host_cache: bool,
) -> std::io::Result<PathBuf> {
    let overlay = scratch.path().join("cargo-overlay");
    let scratch_dir =
        cap_std::fs::Dir::open_ambient_dir(scratch.path(), cap_std::ambient_authority())?;
    scratch_dir.create_dir("cargo-overlay")?;
    let overlay_dir = scratch_dir.open_dir("cargo-overlay")?;
    if let Some(host) = host_cargo_home(environment, workspace, Some(private_cache_base)) {
        sync_cargo_config(&host, &overlay_dir)?;
    }
    for name in ["registry", "git"] {
        let source = if read_host_cache {
            host_cargo_home(environment, workspace, Some(private_cache_base))
                .map(|host| host.join(name))
        } else {
            Some(tool_cache.join("cargo").join(name))
        };
        let Some(source) = source else { continue };
        let source = source.canonicalize().ok();
        if let Some(source) = source.filter(|source| !source.starts_with(workspace)) {
            replace_with_readonly_link(&overlay_dir, &source, name)?;
        }
    }
    Ok(overlay)
}

fn prepare_npm_cache(
    environment: &leveler_core::EnvSnapshot,
    scratch: &tempfile::TempDir,
    tool_cache: &Path,
    workspace: &Path,
    read_host_cache: bool,
) -> std::io::Result<PathBuf> {
    let persistent = tool_cache.join("npm");
    let Some(host) = host_npm_cache(environment, workspace) else {
        return Ok(persistent);
    };
    if !read_host_cache {
        return Ok(persistent);
    }

    let overlay = scratch.path().join("npm-overlay");
    let scratch_dir =
        cap_std::fs::Dir::open_ambient_dir(scratch.path(), cap_std::ambient_authority())?;
    scratch_dir.create_dir("npm-overlay")?;
    let overlay_dir = scratch_dir.open_dir("npm-overlay")?;
    overlay_dir.create_dir("_logs")?;
    let content_cache = host
        .join("_cacache")
        .canonicalize()
        .ok()
        .filter(|path| !path.starts_with(workspace));
    if let Some(content_cache) = content_cache {
        replace_with_readonly_link(&overlay_dir, &content_cache, "_cacache")?;
    }
    Ok(overlay)
}

/// Redirect temp files into per-command scratch and write-heavy tool state into
/// the Leveler-owned, per-workspace cache. Host HOME and toolchain trees remain
/// readable, but are no longer writable.
pub(crate) fn apply_sandbox_environment(cmd: &mut Command, paths: &SandboxPaths) {
    let private_tmp = paths.scratch_path().join("tmp");
    for name in ["TMPDIR", "TMP", "TEMP"] {
        cmd.env(name, &private_tmp);
    }
    let cache_variables = [
        ("GOCACHE", "go/build"),
        ("GOPATH", "go/path"),
        ("YARN_CACHE_FOLDER", "yarn"),
        ("PNPM_HOME", "pnpm"),
        ("PIP_CACHE_DIR", "pip"),
        ("UV_CACHE_DIR", "uv"),
        ("XDG_CACHE_HOME", "xdg-cache"),
    ];
    for (name, relative) in cache_variables {
        cmd.env(name, paths.tool_cache_path().join(relative));
    }
    cmd.env("CARGO_HOME", &paths.cargo_home);
    cmd.env("GOMODCACHE", &paths.go_mod_cache);
    cmd.env("npm_config_cache", &paths.npm_cache);
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn cargo_config_fifo_and_symlink_are_never_followed() {
        let host = tempfile::tempdir().unwrap();
        let private = tempfile::tempdir().unwrap();
        let fifo = host.path().join("config.toml");
        let status = std::process::Command::new("mkfifo")
            .arg(&fifo)
            .status()
            .expect("mkfifo must be available on Unix");
        assert!(status.success());
        let outside = host.path().join("outside-credentials");
        std::fs::write(&outside, "[registry]\ntoken = 'secret'\n").unwrap();
        std::os::unix::fs::symlink(&outside, host.path().join("credentials.toml")).unwrap();

        let private_dir =
            cap_std::fs::Dir::open_ambient_dir(private.path(), cap_std::ambient_authority())
                .unwrap();
        let started = std::time::Instant::now();
        sync_cargo_config(host.path(), &private_dir).unwrap();
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "opening a hostile FIFO must not block"
        );
        assert!(!private.path().join("config.toml").exists());
        assert!(!private.path().join("credentials.toml").exists());
    }
}
