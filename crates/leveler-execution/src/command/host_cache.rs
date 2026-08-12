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
    /// Wrapper environment variables to blank out for children, because the
    /// wrapper Cargo would actually pick here is a known compilation cache.
    /// Cleaning the copied host config is not enough on its own: Cargo also
    /// reads every ancestor `.cargo/config.toml` above the workspace, and the
    /// environment outranks all of them — so the decision is made against the
    /// EFFECTIVE configuration and applied where it cannot be undone.
    wrapper_env_overrides: Vec<&'static str>,
    go_mod_cache: PathBuf,
    npm_cache: PathBuf,
    cache_write_roots: Vec<PathBuf>,
    /// OS lease on `<scratch>.lock`, held for this scratch's lifetime. Its Drop
    /// removes the lock file; the flock releases when the file handle drops
    /// (including on crash, where the reaper reclaims the orphan). Kept last so
    /// it drops after `scratch`.
    _lease: SandboxLeaseGuard,
}

/// RAII guard tying a per-command scratch dir to an exclusive advisory lock on
/// its sidecar `<name>.lock`. A live command holds the lock for its whole
/// lifetime; the reaper treats a lock it can acquire as proof the owner died.
struct SandboxLeaseGuard {
    lock_path: PathBuf,
    _lock: std::fs::File,
}

impl Drop for SandboxLeaseGuard {
    fn drop(&mut self) {
        // Graceful path: remove the sidecar lock file. The flock is released
        // when `_lock` drops right after. (On a crash this Drop never runs, so
        // the lock file lingers and the reaper reclaims it.)
        let _ = std::fs::remove_file(&self.lock_path);
    }
}

/// Acquire an exclusive lease for a freshly-created scratch dir: open (create)
/// `<scratch>.lock` beside it and flock it. The lock is brand-new and
/// uncontended, so this never blocks; a failure is surfaced rather than
/// silently proceeding lockless.
fn acquire_sandbox_lease(scratch_dir: &Path) -> std::io::Result<SandboxLeaseGuard> {
    let lock_path = scratch_dir.with_extension("lock");
    let lock = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(&lock_path)?;
    fs2::FileExt::try_lock_exclusive(&lock)?;
    Ok(SandboxLeaseGuard {
        lock_path,
        _lock: lock,
    })
}

/// Reclaim crash-orphaned scratch dirs under `sandboxes_root`: for each
/// `<name>.lock` whose flock we can acquire, the owner is gone, so remove its
/// `<name>` tree and the lock. Fail-closed — a lock we cannot open or acquire
/// (owner alive), or any I/O hiccup, leaves the entry untouched. Opportunistic:
/// the caller runs it once per process, never on a timer.
fn reap_orphaned_sandboxes(sandboxes_root: &Path) {
    let Ok(entries) = std::fs::read_dir(sandboxes_root) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        let Some(stem) = name.strip_suffix(".lock") else {
            continue;
        };
        let Ok(lock) = std::fs::OpenOptions::new().write(true).open(&path) else {
            continue;
        };
        if fs2::FileExt::try_lock_exclusive(&lock).is_err() {
            // Owner still holds the lease — leave everything alone.
            continue;
        }
        let _ = std::fs::remove_dir_all(sandboxes_root.join(stem));
        let _ = fs2::FileExt::unlock(&lock);
        drop(lock);
        let _ = std::fs::remove_file(&path);
    }
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
    // Scratch dirs live under `run/sandboxes/` beneath the same stable,
    // workspace-external owner (never TMPDIR, which may sit in or be poisoned by
    // the workspace, and never the owner root itself). Reap crash-orphans once
    // per process — opportunistically, before creating this command's dir.
    let sandboxes_root =
        ensure_real_private_chain(&owner, &owner_path, Path::new("run/sandboxes"))?;
    static REAP_ONCE: std::sync::Once = std::sync::Once::new();
    REAP_ONCE.call_once(|| reap_orphaned_sandboxes(&sandboxes_root));
    let scratch = tempfile::Builder::new()
        .prefix("codeleveler-sandbox-")
        .tempdir_in(&sandboxes_root)?;
    let lease = acquire_sandbox_lease(scratch.path())?;
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
    let wrapper_env_overrides = wrapper_env_overrides(
        environment,
        &workspace,
        host_cargo_home(environment, &workspace, Some(&cache_base)).as_deref(),
    );
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
        wrapper_env_overrides,
        go_mod_cache,
        npm_cache,
        cache_write_roots,
        _lease: lease,
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

/// Rustc wrappers that are purely compilation CACHES: they hash the
/// invocation, serve a stored artifact when it matches, and otherwise exec the
/// real rustc. Dropping one changes build SPEED, never build OUTPUT — which is
/// why these, and only these, may be neutralized when the host's Cargo
/// configuration is inherited into the sandbox.
///
/// Any other wrapper is left exactly as the user configured it: a wrapper can
/// carry real semantics (instrumentation, cross-compilation, a custom
/// compiler), and silently removing one would change what the build means.
const CACHE_ONLY_RUSTC_WRAPPERS: &[&str] = &["sccache", "cachepot"];

/// Whether a `rustc-wrapper` value names one of the known cache-only wrappers,
/// whether it was written as a bare command or an absolute path.
fn is_cache_only_wrapper(value: &str) -> bool {
    // A config is portable text: it may name the wrapper bare, or by a path
    // written with either separator regardless of the host we parse it on.
    let stem = value
        .trim()
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or("")
        .trim_end_matches(".exe")
        .to_ascii_lowercase();
    CACHE_ONLY_RUSTC_WRAPPERS.contains(&stem.as_str())
}

/// The two Cargo settings that can put a program in front of rustc, and the
/// environment variable that overrides each.
const RUSTC_WRAPPER_KEYS: &[(&str, &str)] = &[
    ("rustc-wrapper", "RUSTC_WRAPPER"),
    ("rustc-workspace-wrapper", "RUSTC_WORKSPACE_WRAPPER"),
];

/// Read `build.<key>` out of one Cargo config file. A deliberately small
/// reader: this needs two scalars, not a config system.
fn wrapper_in_config(text: &str, key: &str) -> Option<String> {
    let mut in_build = false;
    for line in text.lines() {
        let line = line.trim();
        if line.starts_with('#') {
            continue;
        }
        if let Some(header) = line.strip_prefix('[') {
            in_build = header.trim_end_matches(']').trim() == "build";
            continue;
        }
        if !in_build {
            continue;
        }
        if let Some((name, value)) = line.split_once('=')
            && name.trim() == key
        {
            let value = value.trim().trim_matches(['"', '\'']).trim();
            return (!value.is_empty()).then(|| value.to_string());
        }
    }
    None
}

/// The wrapper Cargo would actually use for a build in `workspace`.
///
/// Cargo's precedence, narrowed to this one setting: the environment wins,
/// then the nearest `.cargo/config(.toml)` walking up from the working
/// directory, and `$CARGO_HOME`'s config last. Nearest wins matters — an
/// outer directory naming a cache says nothing if a closer one replaces it
/// with a wrapper that carries real build semantics.
fn effective_rustc_wrapper(
    environment: &leveler_core::EnvSnapshot,
    workspace: &Path,
    cargo_home: Option<&Path>,
    key: &str,
    env_name: &str,
) -> Option<String> {
    if let Some(value) = environment.var_os(env_name) {
        let value = value.to_string_lossy().trim().to_string();
        if !value.is_empty() {
            return Some(value);
        }
    }
    let config_dirs = workspace
        .ancestors()
        .map(|dir| dir.join(".cargo"))
        .chain(cargo_home.map(Path::to_path_buf));
    for dir in config_dirs {
        for name in ["config.toml", "config"] {
            if let Ok(text) = std::fs::read_to_string(dir.join(name))
                && let Some(value) = wrapper_in_config(&text, key)
            {
                return Some(value);
            }
        }
    }
    None
}

/// The wrapper environment variables this sandbox must blank out: the ones
/// whose EFFECTIVE value is a known compilation cache. An unknown wrapper is
/// never overridden, whatever an outer directory happens to say.
fn wrapper_env_overrides(
    environment: &leveler_core::EnvSnapshot,
    workspace: &Path,
    cargo_home: Option<&Path>,
) -> Vec<&'static str> {
    RUSTC_WRAPPER_KEYS
        .iter()
        .filter(|(key, env_name)| {
            effective_rustc_wrapper(environment, workspace, cargo_home, key, env_name)
                .is_some_and(|value| is_cache_only_wrapper(&value))
        })
        .map(|(_, env_name)| *env_name)
        .collect()
}

/// Neutralize inherited cache-only rustc wrappers in a Cargo config.
///
/// A wrapper the host configured globally is a daemon with its own runtime
/// state. Inside the sandbox it is handed a per-command temp root that is
/// reclaimed the moment the command ends, while the daemon it spawned lives
/// on — so a later compilation asks a surviving server to write under a
/// directory that no longer exists and the build fails for a reason that has
/// nothing to do with the code. A verification gate must never fail that way,
/// so the inherited setting is removed explicitly, with the reason left in
/// place, rather than imported and left to break.
fn neutralize_cache_only_wrappers(config: &str) -> String {
    let mut out = String::with_capacity(config.len());
    for line in config.split_inclusive('\n') {
        let trimmed = line.trim_start();
        let key_end = trimmed.find('=');
        let is_wrapper_key = key_end.is_some_and(|end| {
            matches!(
                trimmed[..end].trim(),
                "rustc-wrapper" | "rustc-workspace-wrapper"
            )
        });
        let value = key_end
            .map(|end| {
                trimmed[end + 1..]
                    .trim()
                    .trim_matches(['"', '\'', '\r', '\n'])
            })
            .unwrap_or("");
        if is_wrapper_key && is_cache_only_wrapper(value) {
            out.push_str(&format!(
                "# {} (compilation cache) removed by CodeLeveler: its daemon outlives the\n\
                 # sandboxed command that starts it and cannot keep that command's temp\n\
                 # directory. A cache changes build speed, not build output.\n\
                 # original: {}",
                value,
                line.trim_end_matches(['\r', '\n'])
            ));
            out.push('\n');
            continue;
        }
        out.push_str(line);
    }
    out
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
                // Credentials are copied verbatim; a config may carry an
                // inherited toolchain wrapper the sandbox cannot host.
                if matches!(name, "config" | "config.toml")
                    && let Ok(text) = std::str::from_utf8(&bytes)
                {
                    private.write(name, neutralize_cache_only_wrappers(text))?;
                } else {
                    private.write(name, bytes)?;
                }
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
    // The env form of the same inherited setting. `Command::env_remove` only
    // affects this child, never the host session.
    // Empty, not removed: Cargo reads these ahead of every config file, so no
    // ancestor `.cargo/config.toml` can reintroduce a wrapper the sandbox
    // cannot host.
    for name in &paths.wrapper_env_overrides {
        cmd.env(name, "");
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

    /// The wrapper settings a Cargo config actually applies — comments carry
    /// the neutralization note and must not be mistaken for live settings.
    fn active_wrapper_lines(config: &str) -> Vec<&str> {
        config
            .lines()
            .map(str::trim)
            .filter(|line| !line.starts_with('#'))
            .filter(|line| {
                line.starts_with("rustc-wrapper") || line.starts_with("rustc-workspace-wrapper")
            })
            .collect()
    }

    /// The reader only answers for `build.<key>`: a same-named key under
    /// another table, or a commented-out line, is not a setting.
    #[test]
    fn the_config_reader_only_sees_the_build_table() {
        assert_eq!(
            wrapper_in_config("[build]\nrustc-wrapper = \"sccache\"\n", "rustc-wrapper"),
            Some("sccache".to_string())
        );
        assert_eq!(
            wrapper_in_config("[alias]\nrustc-wrapper = \"sccache\"\n", "rustc-wrapper"),
            None
        );
        assert_eq!(
            wrapper_in_config("[build]\n# rustc-wrapper = \"sccache\"\n", "rustc-wrapper"),
            None
        );
        assert_eq!(
            wrapper_in_config("[build]\nrustc-wrapper = \"\"\n", "rustc-wrapper"),
            None,
            "an explicitly empty setting is not a wrapper"
        );
        assert_eq!(
            wrapper_in_config(
                "[build]\nrustc-workspace-wrapper = 'cachepot'\n",
                "rustc-workspace-wrapper"
            ),
            Some("cachepot".to_string())
        );
    }

    /// A cache wrapper is dropped, and the reason survives in the file so the
    /// sandbox's config is self-explaining rather than mysteriously different
    /// from the host's.
    #[test]
    fn a_cache_only_wrapper_is_neutralized_with_its_reason() {
        let out = neutralize_cache_only_wrappers(
            "[build]\nrustc-wrapper = \"sccache\"\nincremental = true\n",
        );
        assert!(active_wrapper_lines(&out).is_empty(), "{out}");
        assert!(out.contains("removed by CodeLeveler"), "{out}");
        assert!(
            out.contains("original: rustc-wrapper = \"sccache\""),
            "{out}"
        );
        // Everything else survives untouched.
        assert!(out.contains("[build]"));
        assert!(out.contains("incremental = true"));
    }

    #[test]
    fn an_absolute_path_to_a_cache_wrapper_is_recognized() {
        for value in [
            "/opt/homebrew/bin/sccache",
            "/usr/local/bin/cachepot",
            "C:\\tools\\sccache.exe",
        ] {
            let out =
                neutralize_cache_only_wrappers(&format!("[build]\nrustc-wrapper = \"{value}\"\n"));
            assert!(active_wrapper_lines(&out).is_empty(), "{value}: {out}");
        }
    }

    #[test]
    fn the_workspace_wrapper_key_is_covered_too() {
        let out =
            neutralize_cache_only_wrappers("[build]\nrustc-workspace-wrapper = \"sccache\"\n");
        assert!(active_wrapper_lines(&out).is_empty(), "{out}");
    }

    /// The narrow part of the contract: a wrapper we cannot prove is a cache
    /// may carry real semantics (instrumentation, a custom compiler), so it is
    /// inherited exactly as written — even though it may then fail in the
    /// sandbox. Guessing here would silently change what the build means.
    #[test]
    fn an_unknown_wrapper_is_left_exactly_as_configured() {
        let config = "[build]\nrustc-wrapper = \"my-instrumenting-wrapper\"\n";
        assert_eq!(neutralize_cache_only_wrappers(config), config);
        assert_eq!(active_wrapper_lines(config).len(), 1);
        let by_path = "[build]\nrustc-wrapper = \"/opt/team/bin/coverage-rustc\"\n";
        assert_eq!(neutralize_cache_only_wrappers(by_path), by_path);
    }

    #[test]
    fn a_config_without_any_wrapper_is_byte_identical() {
        let config = "[net]\nretry = 3\n\n[build]\nincremental = true\njobs = 4\n";
        assert_eq!(neutralize_cache_only_wrappers(config), config);
    }

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

    // ── sandbox lease + reaper (D2) ─────────────────────────────────────────

    /// S1: a lease creates and holds `<scratch>.lock`; dropping it removes the
    /// lock file (the graceful RAII path).
    #[test]
    fn lease_holds_then_removes_its_lock_on_drop() {
        let root = tempfile::tempdir().unwrap();
        let scratch = root.path().join("codeleveler-sandbox-abc");
        std::fs::create_dir(&scratch).unwrap();
        let lock = scratch.with_extension("lock");

        let lease = acquire_sandbox_lease(&scratch).unwrap();
        assert!(lock.exists(), "the lease creates its sidecar lock");
        drop(lease);
        assert!(!lock.exists(), "dropping the lease removes the lock file");
    }

    /// S2: the reaper reclaims an orphan whose lock is free (owner gone) —
    /// removing both the scratch tree and the lock.
    #[test]
    fn reaper_reclaims_an_orphan_with_a_free_lock() {
        let root = tempfile::tempdir().unwrap();
        let scratch = root.path().join("codeleveler-sandbox-dead");
        std::fs::create_dir(&scratch).unwrap();
        std::fs::write(scratch.join("residue"), b"x").unwrap();
        // Simulate a crashed owner: lock file left behind, no live flock.
        std::fs::write(scratch.with_extension("lock"), b"").unwrap();

        reap_orphaned_sandboxes(root.path());

        assert!(!scratch.exists(), "orphan scratch tree is removed");
        assert!(
            !scratch.with_extension("lock").exists(),
            "orphan lock is removed"
        );
    }

    /// S3: a live lease is never reaped — its held flock is the proof of life.
    #[test]
    fn reaper_leaves_a_live_lease_untouched() {
        let root = tempfile::tempdir().unwrap();
        let scratch = root.path().join("codeleveler-sandbox-live");
        std::fs::create_dir(&scratch).unwrap();
        let _lease = acquire_sandbox_lease(&scratch).unwrap();

        reap_orphaned_sandboxes(root.path());

        assert!(scratch.exists(), "a live-leased scratch must survive");
        assert!(scratch.with_extension("lock").exists());
    }

    /// S4: fail-closed — a scratch dir with no lock sidecar is left alone rather
    /// than deleted on a guess.
    #[test]
    fn reaper_is_fail_closed_without_a_lock() {
        let root = tempfile::tempdir().unwrap();
        let scratch = root.path().join("codeleveler-sandbox-nolock");
        std::fs::create_dir(&scratch).unwrap();
        std::fs::write(scratch.join("keep"), b"x").unwrap();

        reap_orphaned_sandboxes(root.path());

        assert!(
            scratch.exists(),
            "a lock-less dir is ambiguous and must be left untouched"
        );
    }
}
