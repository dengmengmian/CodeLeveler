//! Filesystem layout: where CodeLeveler reads config and writes state.

use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};

const REPOSITORY_OWNER_FILE: &str = ".repository-root";

/// Resolved paths for a CodeLeveler run rooted at a repository.
#[derive(Debug, Clone)]
pub struct Layout {
    /// Repository root (the current working directory by default).
    pub repo_root: PathBuf,
    /// Directory holding provider/model/policy config bundles.
    pub config_dir: PathBuf,
    /// Runtime-state directory for this repo. Lives OUTSIDE the project — under
    /// the global CodeLeveler home, keyed by the repo path — so a checkout stays
    /// clean and never needs `.gitignore`.
    /// Holds the session DB, composer drafts, and the image store.
    pub state_dir: PathBuf,
}

impl Layout {
    /// Build a layout for `repo_root`, resolving the config directory.
    ///
    /// The config directory is taken from `config_dir_override`, else the
    /// `LEVELER_CONFIG_DIR` env var, else `<repo>/configs` (dev layout).
    ///
    /// Runtime state goes to `$LEVELER_HOME/projects/<encoded-repo-path>/`
    /// (default `~/.leveler/projects/…`), NOT `<repo>/.leveler` — the project
    /// dir stays clean. User-authored config (`<repo>/.leveler/config.yaml`,
    /// `rules/`, `skills/`, `instructions.md`) still lives in the repo.
    pub fn resolve(repo_root: PathBuf, config_dir_override: Option<PathBuf>) -> Self {
        Self::resolve_with_environment(repo_root, config_dir_override, leveler_core::environment())
    }

    pub fn resolve_with_environment(
        repo_root: PathBuf,
        config_dir_override: Option<PathBuf>,
        environment: &leveler_core::EnvSnapshot,
    ) -> Self {
        // Canonicalize so macOS `/var` vs `/private/var` (and other symlink
        // roots) share one state namespace — otherwise sessions list/resume
        // miss the DB written under the other spelling.
        let repo_root = std::fs::canonicalize(&repo_root).unwrap_or(repo_root);
        let config_dir = config_dir_override
            .or_else(|| environment.var_os("LEVELER_CONFIG_DIR").map(PathBuf::from))
            .unwrap_or_else(|| repo_root.join("configs"));
        let state_dir = repo_state_dir_in(&leveler_home(environment), &repo_root);
        Self {
            repo_root,
            config_dir,
            state_dir,
        }
    }

    pub fn providers_dir(&self) -> PathBuf {
        self.config_dir.join("providers")
    }

    pub fn models_dir(&self) -> PathBuf {
        self.config_dir.join("models")
    }

    /// SQLite database path (`<state_dir>/sessions.db`, now under the global home).
    pub fn database_path(&self) -> PathBuf {
        self.state_dir.join("sessions.db")
    }

    /// Per-repository local runtime endpoint.
    ///
    /// NOT under `state_dir`: Unix socket paths must stay below `SUN_LEN`
    /// (~104 bytes on macOS) and the hashed state-dir name grows with the
    /// repository path — deep repos overflow it. The socket instead lives in
    /// a short per-home dir, keyed by the same 16-hex repo-path hash, so any
    /// process resolving the same repository derives the same endpoint.
    pub fn socket_path(&self) -> PathBuf {
        let hash = path_hash(&self.repo_root.to_string_lossy());
        match self.state_dir.parent().and_then(Path::parent) {
            Some(home) => home.join("sock").join(format!("{hash}.sock")),
            // A state dir with no home above it (hand-built layouts): keep
            // the socket beside the state.
            None => self.state_dir.join("runtime.sock"),
        }
    }

    /// Durable project memory root (`active/` + `archive/` JSON entries).
    pub fn memory_dir(&self) -> PathBuf {
        self.state_dir.join("memory")
    }

    /// ApproveAlways permission-rules file (`<state_dir>/permissions.yaml`,
    /// next to `sessions.db`) — machine-written per-user state, kept out of
    /// the repo.
    pub fn permissions_path(&self) -> PathBuf {
        self.state_dir.join("permissions.yaml")
    }
}

/// The global CodeLeveler home: `$LEVELER_HOME`, else `$HOME/.leveler` (or
/// `%USERPROFILE%\.leveler` on Windows), else a process-local temp directory.
fn leveler_home(environment: &leveler_core::EnvSnapshot) -> PathBuf {
    resolve_leveler_home(
        environment.var_os("LEVELER_HOME"),
        environment.var_os("HOME"),
        environment.var_os("USERPROFILE"),
        environment.temp_dir().to_path_buf(),
    )
}

fn resolve_leveler_home(
    leveler_home: Option<std::ffi::OsString>,
    home: Option<std::ffi::OsString>,
    userprofile: Option<std::ffi::OsString>,
    temp_dir: PathBuf,
) -> PathBuf {
    // Home-resolution order (incl. USERPROFILE) is shared via leveler-core; this
    // adds the process-local temp fallback the runtime state dir requires.
    leveler_core::leveler_home_dir_from(|k| match k {
        "LEVELER_HOME" => leveler_home.clone(),
        "HOME" => home.clone(),
        "USERPROFILE" => userprofile.clone(),
        _ => None,
    })
    .unwrap_or_else(|| temp_dir.join(format!("leveler-{}", std::process::id())))
}

/// Advisory write-lock path for a workspace file: `<home>/locks/<hash>.lock`.
///
/// Lock files live under the global home — never next to the target — so
/// workspaces don't accumulate `.<name>.leveler-lock` residue. Keyed by a
/// stable hash of the absolute target path, so independent CodeLeveler
/// processes editing the same file agree on the same lock.
pub fn target_lock_path(environment: &leveler_core::EnvSnapshot, target_abs: &Path) -> PathBuf {
    leveler_home(environment)
        .join("locks")
        .join(format!("{}.lock", path_hash(&target_abs.to_string_lossy())))
}

/// The runtime-state dir for a repo under `home`: `<home>/projects/<encoded>`.
///
/// Migrates a pre-hash legacy directory only when an ownership marker proves
/// it belongs to this repository. An unmarked legacy slug is ambiguous because
/// multiple paths may have collapsed to it, so it is deliberately left alone.
fn repo_state_dir_in(home: &Path, repo_root: &Path) -> PathBuf {
    let projects = home.join("projects");
    let encoded = encode_repo_path(repo_root);
    let new_dir = projects.join(&encoded);
    let legacy = encode_repo_path_legacy(repo_root);
    if legacy != encoded {
        let legacy_dir = projects.join(&legacy);
        migrate_legacy_state_dir(&legacy_dir, &new_dir, repo_root);
    }
    write_owner_marker_if_directory_exists(&new_dir, repo_root);
    new_dir
}

/// Encode an absolute repo path into a single directory-name segment:
/// readable slug + short stable hash of the full path.
///
/// The hash disambiguates paths that collapse to the same slug when
/// non-alphanumeric characters are replaced (e.g. `/tmp/a-b` vs `/tmp/a/b`).
/// Example: `/Users/me/app` → `-Users-me-app-a1b2c3d4e5f6g7h8`.
pub fn encode_repo_path(repo_root: &Path) -> String {
    let path = repo_root.to_string_lossy();
    let slug = path
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect::<String>();
    let hash = path_hash(&path);
    format!("{slug}-{hash}")
}

/// Pre-hash encoding (non-alphanumeric → `-` only). Kept for one-shot migration.
fn encode_repo_path_legacy(repo_root: &Path) -> String {
    repo_root
        .to_string_lossy()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect()
}

/// First 16 hex chars of SHA-256 over the UTF-8 path bytes (stable across runs).
fn path_hash(path: &str) -> String {
    let digest = Sha256::digest(path.as_bytes());
    let mut hex = String::with_capacity(16);
    for byte in digest.iter().take(8) {
        hex.push_str(&format!("{byte:02x}"));
    }
    hex
}

/// If only the legacy dir exists, rename it to the new hashed path. Never merge
/// when both exist — that would mix two projects that previously collided.
fn migrate_legacy_state_dir(legacy: &Path, new_dir: &Path, repo_root: &Path) {
    if !legacy.exists() || new_dir.exists() || legacy == new_dir {
        return;
    }
    let expected_owner = repo_root.to_string_lossy();
    let owner = std::fs::read_to_string(legacy.join(REPOSITORY_OWNER_FILE));
    if !matches!(owner.as_deref().map(str::trim), Ok(owner) if owner == expected_owner) {
        return;
    }
    if let Some(parent) = new_dir.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    // Best-effort: if rename fails (cross-device, permissions), the next
    // resolve creates a fresh hashed dir and the legacy data is left alone.
    let _ = std::fs::rename(legacy, new_dir);
}

/// Explicitly migrate an unmarked pre-hash state directory after its owner has
/// been confirmed by the caller (for example through an upgrade prompt).
///
/// Automatic resolution never claims an unmarked directory because old slugs
/// can collide. This API is the recovery path for genuine pre-marker installs:
/// it refuses to merge or overwrite an existing hashed directory and returns
/// `Ok(false)` when no legacy directory exists.
pub fn migrate_legacy_repo_state(home: &Path, repo_root: &Path) -> std::io::Result<bool> {
    let (legacy, new_dir) = legacy_repo_state_paths(home, repo_root);
    if !legacy.exists() {
        return Ok(false);
    }
    if new_dir.exists() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::AlreadyExists,
            format!("refusing to merge into {}", new_dir.display()),
        ));
    }
    if let Some(projects) = new_dir.parent() {
        std::fs::create_dir_all(projects)?;
    }
    std::fs::rename(&legacy, &new_dir)?;
    std::fs::write(
        new_dir.join(REPOSITORY_OWNER_FILE),
        repo_root.to_string_lossy().as_bytes(),
    )?;
    Ok(true)
}

/// Source and destination used by [`migrate_legacy_repo_state`], exposed so a
/// CLI can show the exact operation before requiring confirmation.
pub fn legacy_repo_state_paths(home: &Path, repo_root: &Path) -> (PathBuf, PathBuf) {
    let projects = home.join("projects");
    (
        projects.join(encode_repo_path_legacy(repo_root)),
        projects.join(encode_repo_path(repo_root)),
    )
}

fn write_owner_marker_if_directory_exists(state_dir: &Path, repo_root: &Path) {
    if !state_dir.is_dir() {
        return;
    }
    let marker = state_dir.join(REPOSITORY_OWNER_FILE);
    if !marker.exists() {
        let _ = std::fs::write(marker, repo_root.to_string_lossy().as_bytes());
    }
}

/// Every repository that has Leveler state under `home`: each
/// `<home>/projects/*/` directory carrying a `.repository-root` marker stores
/// the owning repository's path in that marker. Sorted for deterministic
/// output; unreadable or marker-less directories are skipped. The repository
/// itself may no longer exist — existence is the caller's decision.
pub fn known_repositories(home: &Path) -> Vec<PathBuf> {
    let mut repos: Vec<PathBuf> = match std::fs::read_dir(home.join("projects")) {
        Ok(entries) => entries
            .filter_map(Result::ok)
            .filter_map(|entry| {
                let owner =
                    std::fs::read_to_string(entry.path().join(REPOSITORY_OWNER_FILE)).ok()?;
                let owner = owner.trim();
                if owner.is_empty() {
                    None
                } else {
                    Some(PathBuf::from(owner))
                }
            })
            .collect(),
        Err(_) => Vec::new(),
    };
    repos.sort();
    repos
}

/// Collect YAML files (`*.yaml` / `*.yml`) directly inside `dir`, sorted by name
/// for deterministic loading. Returns an empty vec if the directory is absent.
pub fn yaml_files(dir: &Path) -> Vec<PathBuf> {
    let mut files: Vec<PathBuf> = match std::fs::read_dir(dir) {
        Ok(entries) => entries
            .filter_map(Result::ok)
            .map(|e| e.path())
            .filter(|p| {
                matches!(
                    p.extension().and_then(|e| e.to_str()),
                    Some("yaml") | Some("yml")
                )
            })
            .collect(),
        Err(_) => Vec::new(),
    };
    files.sort();
    files
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn defaults_config_dir_to_repo_configs() {
        let layout = Layout::resolve(PathBuf::from("/repo"), None);
        // Only assert when the env override is not set in this environment.
        if std::env::var_os("LEVELER_CONFIG_DIR").is_none() {
            assert_eq!(layout.config_dir, PathBuf::from("/repo/configs"));
        }
    }

    #[test]
    fn runtime_state_lives_outside_the_repo() {
        let layout = Layout::resolve(PathBuf::from("/repo"), None);
        let db = layout.database_path();
        // The DB no longer lands in the project — it's under the global home,
        // namespaced by the repo path.
        assert!(db.ends_with("sessions.db"), "{db:?}");
        assert!(
            !db.starts_with("/repo"),
            "state must not be in the repo: {db:?}"
        );
        assert!(
            db.to_string_lossy().contains("projects"),
            "namespaced under projects/: {db:?}"
        );
    }

    #[test]
    fn target_lock_path_lives_under_home_locks_never_in_the_workspace() {
        let environment = leveler_core::EnvSnapshot::new(
            [(
                std::ffi::OsString::from("LEVELER_HOME"),
                std::ffi::OsString::from("/home/x/.leveler"),
            )],
            PathBuf::from("/"),
            PathBuf::from("/tmp"),
        );
        let lock = target_lock_path(&environment, Path::new("/repo/src/lib.rs"));
        assert_eq!(
            lock,
            PathBuf::from(format!(
                "/home/x/.leveler/locks/{}.lock",
                path_hash("/repo/src/lib.rs")
            ))
        );
    }

    #[test]
    fn socket_lives_in_the_short_per_home_dir_keyed_by_repo_hash() {
        let layout = Layout {
            repo_root: PathBuf::from("/repo"),
            config_dir: PathBuf::from("/config"),
            state_dir: PathBuf::from("/home/x/.leveler/projects/-repo-abcdef1234567890"),
        };
        let socket = layout.socket_path();
        assert_eq!(
            socket,
            PathBuf::from(format!("/home/x/.leveler/sock/{}.sock", path_hash("/repo")))
        );
    }

    #[test]
    fn socket_path_stays_under_sun_len_for_deep_repositories() {
        // The regression this guards: state-dir-based sockets overflowed
        // macOS's ~104-byte sun_path limit for deeply nested repos.
        let repo = "/Users/someone/Develop/app/codeleveler/fixtures/repos/commander";
        let layout = Layout::resolve_with_environment(
            PathBuf::from(repo),
            None,
            &leveler_core::EnvSnapshot::new(
                [(
                    std::ffi::OsString::from("HOME"),
                    std::ffi::OsString::from("/Users/someone"),
                )],
                PathBuf::from("/"),
                PathBuf::from("/tmp"),
            ),
        );
        let socket = layout.socket_path();
        assert!(
            socket.as_os_str().len() < 100,
            "socket path must fit sun_path: {} ({} bytes)",
            socket.display(),
            socket.as_os_str().len()
        );
    }

    #[test]
    fn missing_home_never_falls_back_to_the_working_directory() {
        let home = resolve_leveler_home(None, None, None, PathBuf::from("/system/tmp"));
        assert_eq!(
            home,
            PathBuf::from(format!("/system/tmp/leveler-{}", std::process::id())),
            "missing HOME must use isolated temp state instead of the cwd",
        );
    }

    #[test]
    fn empty_home_values_also_use_temp_state() {
        let empty = Some(std::ffi::OsString::new());
        let home = resolve_leveler_home(
            empty.clone(),
            empty.clone(),
            empty,
            PathBuf::from("/system/tmp"),
        );
        assert_eq!(
            home,
            PathBuf::from(format!("/system/tmp/leveler-{}", std::process::id())),
        );
    }

    #[test]
    fn encodes_repo_path_and_namespaces_state() {
        let encoded = encode_repo_path(Path::new("/Users/me/app"));
        assert!(
            encoded.starts_with("-Users-me-app-"),
            "readable slug prefix: {encoded}"
        );
        assert_eq!(
            encoded.len(),
            "-Users-me-app-".len() + 16,
            "slug + 16-char hash: {encoded}"
        );
        let dir = repo_state_dir_in(Path::new("/home/x/.leveler"), Path::new("/repo/foo.bar"));
        let name = dir.file_name().unwrap().to_string_lossy();
        assert!(
            name.starts_with("-repo-foo-bar-"),
            "namespaced under projects/: {dir:?}"
        );
        assert_eq!(
            dir.parent().unwrap(),
            Path::new("/home/x/.leveler/projects")
        );
    }

    #[test]
    fn distinct_paths_that_share_a_slug_get_distinct_state_dirs() {
        // Both collapse to the same legacy slug `-tmp-a-b`.
        let a = encode_repo_path(Path::new("/tmp/a-b"));
        let b = encode_repo_path(Path::new("/tmp/a/b"));
        assert_ne!(a, b, "must not collide: {a} vs {b}");
        assert!(a.starts_with("-tmp-a-b-"));
        assert!(b.starts_with("-tmp-a-b-"));
    }

    #[test]
    fn encode_is_stable_for_the_same_path() {
        let once = encode_repo_path(Path::new("/Users/me/app"));
        let twice = encode_repo_path(Path::new("/Users/me/app"));
        assert_eq!(once, twice);
    }

    #[test]
    fn migrates_legacy_state_dir_when_new_path_is_free() {
        let base = std::env::temp_dir().join(format!(
            "leveler-migrate-state-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let home = base.join("home");
        let repo = Path::new("/Users/me/app");
        let legacy = home.join("projects").join(encode_repo_path_legacy(repo));
        fs::create_dir_all(&legacy).unwrap();
        fs::write(legacy.join("sessions.db"), b"legacy-db").unwrap();
        fs::write(
            legacy.join(REPOSITORY_OWNER_FILE),
            repo.to_string_lossy().as_bytes(),
        )
        .unwrap();

        let dir = repo_state_dir_in(&home, repo);
        assert_eq!(dir, home.join("projects").join(encode_repo_path(repo)));
        assert!(dir.join("sessions.db").exists(), "migrated db missing");
        assert!(!legacy.exists(), "legacy dir should be renamed away");
        assert_eq!(fs::read(dir.join("sessions.db")).unwrap(), b"legacy-db");
        fs::remove_dir_all(&base).ok();
    }

    #[test]
    fn does_not_claim_unmarked_legacy_state() {
        let base = std::env::temp_dir().join(format!(
            "leveler-ambiguous-state-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let home = base.join("home");
        let repo = Path::new("/tmp/a-b");
        let legacy = home.join("projects").join(encode_repo_path_legacy(repo));
        fs::create_dir_all(&legacy).unwrap();
        fs::write(legacy.join("sessions.db"), b"unknown-owner").unwrap();

        let new_dir = repo_state_dir_in(&home, repo);
        assert!(
            legacy.exists(),
            "ambiguous legacy data must remain untouched"
        );
        assert!(!new_dir.exists(), "unowned data must not be auto-claimed");
        fs::remove_dir_all(&base).ok();
    }

    #[test]
    fn does_not_claim_legacy_state_owned_by_another_repo() {
        let base = std::env::temp_dir().join(format!(
            "leveler-wrong-owner-state-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let home = base.join("home");
        let repo = Path::new("/tmp/a-b");
        let legacy = home.join("projects").join(encode_repo_path_legacy(repo));
        fs::create_dir_all(&legacy).unwrap();
        fs::write(legacy.join(REPOSITORY_OWNER_FILE), b"/tmp/a/b").unwrap();

        let new_dir = repo_state_dir_in(&home, repo);
        assert!(legacy.exists(), "another repository's state must remain");
        assert!(
            !new_dir.exists(),
            "another repository's state must not move"
        );
        fs::remove_dir_all(&base).ok();
    }

    #[test]
    fn explicit_upgrade_migrates_a_real_pre_marker_directory() {
        let base = std::env::temp_dir().join(format!(
            "leveler-explicit-upgrade-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let home = base.join("home");
        let repo = Path::new("/Users/me/pre-marker");
        let legacy = home.join("projects").join(encode_repo_path_legacy(repo));
        fs::create_dir_all(&legacy).unwrap();
        fs::write(legacy.join("sessions.db"), b"old-sessions").unwrap();

        assert!(migrate_legacy_repo_state(&home, repo).unwrap());
        let new_dir = home.join("projects").join(encode_repo_path(repo));
        assert_eq!(
            fs::read(new_dir.join("sessions.db")).unwrap(),
            b"old-sessions"
        );
        assert_eq!(
            fs::read_to_string(new_dir.join(REPOSITORY_OWNER_FILE)).unwrap(),
            repo.to_string_lossy()
        );
        assert!(!legacy.exists());
        fs::remove_dir_all(&base).ok();
    }

    #[test]
    fn explicit_upgrade_refuses_to_merge_existing_hashed_state() {
        let base = std::env::temp_dir().join(format!(
            "leveler-explicit-no-merge-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let home = base.join("home");
        let repo = Path::new("/tmp/a-b");
        let legacy = home.join("projects").join(encode_repo_path_legacy(repo));
        let new_dir = home.join("projects").join(encode_repo_path(repo));
        fs::create_dir_all(&legacy).unwrap();
        fs::create_dir_all(&new_dir).unwrap();

        let error = migrate_legacy_repo_state(&home, repo).unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::AlreadyExists);
        assert!(legacy.exists());
        assert!(new_dir.exists());
        fs::remove_dir_all(&base).ok();
    }

    #[test]
    fn does_not_merge_when_both_legacy_and_new_exist() {
        let base = std::env::temp_dir().join(format!(
            "leveler-no-merge-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let home = base.join("home");
        let repo = Path::new("/Users/me/app");
        let legacy = home.join("projects").join(encode_repo_path_legacy(repo));
        let neu = home.join("projects").join(encode_repo_path(repo));
        fs::create_dir_all(&legacy).unwrap();
        fs::create_dir_all(&neu).unwrap();
        fs::write(legacy.join("sessions.db"), b"legacy").unwrap();
        fs::write(neu.join("sessions.db"), b"new").unwrap();

        let dir = repo_state_dir_in(&home, repo);
        assert_eq!(dir, neu);
        assert_eq!(fs::read(dir.join("sessions.db")).unwrap(), b"new");
        assert!(
            legacy.exists(),
            "legacy must remain when new already exists"
        );
        fs::remove_dir_all(&base).ok();
    }

    #[test]
    fn resolve_canonicalizes_existing_repo_so_symlink_spellings_share_state() {
        let base = std::env::temp_dir().join(format!(
            "leveler-canon-repo-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&base).unwrap();
        // On macOS temp is often under /var → /private/var. Build a non-canonical
        // spelling by appending `/./` components that canonicalize() removes.
        let non_canon = base.join(".").join("proj");
        fs::create_dir_all(&non_canon).unwrap();
        let via_dot = non_canon.clone();
        let via_plain = base.join("proj");

        let a = Layout::resolve(via_dot, None);
        let b = Layout::resolve(via_plain, None);
        assert_eq!(
            a.state_dir, b.state_dir,
            "state dirs must match after canonicalize:\n  a={:?}\n  b={:?}",
            a.state_dir, b.state_dir
        );
        assert_eq!(a.repo_root, b.repo_root);
        // Clean up.
        fs::remove_dir_all(&base).ok();
    }

    #[test]
    fn override_wins() {
        let layout = Layout::resolve(PathBuf::from("/repo"), Some(PathBuf::from("/custom")));
        assert_eq!(layout.config_dir, PathBuf::from("/custom"));
        assert_eq!(layout.providers_dir(), PathBuf::from("/custom/providers"));
    }

    #[test]
    fn known_repositories_reads_owner_markers() {
        let base = std::env::temp_dir().join(format!(
            "leveler-known-repos-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let projects = base.join("home").join("projects");
        // One properly marked state dir.
        let marked = projects.join("-tmp-app-0123456789abcdef");
        fs::create_dir_all(&marked).unwrap();
        fs::write(marked.join(REPOSITORY_OWNER_FILE), b"/tmp/app\n").unwrap();
        // A marker-less dir and an empty marker are skipped.
        fs::create_dir_all(projects.join("unmarked")).unwrap();
        let empty = projects.join("empty-marker");
        fs::create_dir_all(&empty).unwrap();
        fs::write(empty.join(REPOSITORY_OWNER_FILE), b"  \n").unwrap();

        let repos = known_repositories(&base.join("home"));
        assert_eq!(repos, vec![PathBuf::from("/tmp/app")]);
        fs::remove_dir_all(&base).ok();
    }

    #[test]
    fn known_repositories_returns_empty_without_projects_dir() {
        assert!(known_repositories(Path::new("/does/not/exist")).is_empty());
    }

    #[test]
    fn yaml_files_collects_and_sorts_yaml_and_yml() {
        let dir = std::env::temp_dir().join(format!("leveler-layout-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("b.yaml"), "").unwrap();
        fs::write(dir.join("a.yml"), "").unwrap();
        fs::write(dir.join("c.txt"), "").unwrap();
        let files = yaml_files(&dir);
        assert_eq!(files.len(), 2);
        assert!(files[0].ends_with("a.yml"));
        assert!(files[1].ends_with("b.yaml"));
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn yaml_files_returns_empty_for_missing_dir() {
        let files = yaml_files(Path::new("/does/not/exist"));
        assert!(files.is_empty());
    }

    #[test]
    fn layout_models_dir_derives_from_config() {
        let layout = Layout::resolve(PathBuf::from("/repo"), Some(PathBuf::from("/custom")));
        assert_eq!(layout.models_dir(), PathBuf::from("/custom/models"));
    }
}
