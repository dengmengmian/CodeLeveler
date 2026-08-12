//! Filesystem layout: where CodeLeveler reads config and writes state.

use std::path::{Path, PathBuf};

use leveler_core::LevelerHome;
use sha2::{Digest, Sha256};

/// Ownership marker inside a `<home>/projects/<slug>/` state dir: holds the
/// absolute path of the repository the dir belongs to.
pub const REPOSITORY_OWNER_FILE: &str = ".repository-root";

/// Resolved paths for a CodeLeveler run rooted at a repository.
#[derive(Debug, Clone)]
pub struct Layout {
    /// Repository root (the current working directory by default).
    pub repo_root: PathBuf,
    /// Directory holding provider/model/policy config bundles.
    pub config_dir: PathBuf,
    /// Runtime-state directory for this repo: `<home>/state/projects/<id>/`.
    /// Lives OUTSIDE the project — under the global CodeLeveler home, keyed by
    /// the repo path — so a checkout stays clean and never needs `.gitignore`.
    /// Holds the session DB, memory, composer drafts, and the image store.
    pub state_dir: PathBuf,
    /// The global home this layout's runtime paths derive from (sockets, locks).
    home: LevelerHome,
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
        // `config_dir` (the dev providers/models bundle) is orthogonal to the
        // home namespace — it keeps its own LEVELER_CONFIG_DIR / <repo>/configs
        // resolution and is NOT a LevelerHome path.
        let config_dir = config_dir_override
            .or_else(|| environment.var_os("LEVELER_CONFIG_DIR").map(PathBuf::from))
            .unwrap_or_else(|| repo_root.join("configs"));
        let home = LevelerHome::resolve(environment);
        let state_dir = home.project_state_dir(&encode_repo_path(&repo_root));
        write_owner_marker_if_directory_exists(&state_dir, &repo_root);
        Self {
            repo_root,
            config_dir,
            state_dir,
            home,
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
    /// the short `run/sockets/` dir, keyed by the same 16-hex repo-path hash,
    /// so any process resolving the same repository derives the same endpoint.
    pub fn socket_path(&self) -> PathBuf {
        let hash = path_hash(&self.repo_root.to_string_lossy());
        self.home.sockets_dir().join(format!("{hash}.sock"))
    }

    /// The global home backing this layout's runtime paths.
    pub fn home(&self) -> &LevelerHome {
        &self.home
    }

    /// Build a layout from explicit paths (tests, embedding). The home backing
    /// runtime paths (sockets/locks) is taken as `state_dir`'s parent so they
    /// stay beside the given state rather than the real user home.
    pub fn from_parts(repo_root: PathBuf, config_dir: PathBuf, state_dir: PathBuf) -> Self {
        let home = LevelerHome::from_root(
            state_dir
                .parent()
                .map(Path::to_path_buf)
                .unwrap_or_else(|| state_dir.clone()),
        );
        Self {
            repo_root,
            config_dir,
            state_dir,
            home,
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

/// Advisory write-lock path for a workspace file: `<home>/run/locks/<hash>.lock`.
///
/// Lock files live under the global home — never next to the target — so
/// workspaces don't accumulate `.<name>.leveler-lock` residue. Keyed by a
/// stable hash of the absolute target path, so independent CodeLeveler
/// processes editing the same file agree on the same lock.
pub fn target_lock_path(environment: &leveler_core::EnvSnapshot, target_abs: &Path) -> PathBuf {
    LevelerHome::resolve(environment)
        .locks_dir()
        .join(format!("{}.lock", path_hash(&target_abs.to_string_lossy())))
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

/// First 16 hex chars of SHA-256 over the UTF-8 path bytes (stable across runs).
fn path_hash(path: &str) -> String {
    let digest = Sha256::digest(path.as_bytes());
    let mut hex = String::with_capacity(16);
    for byte in digest.iter().take(8) {
        hex.push_str(&format!("{byte:02x}"));
    }
    hex
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
/// `state/projects/*/` directory carrying a `.repository-root` marker stores
/// the owning repository's path in that marker. Sorted for deterministic
/// output; unreadable or marker-less directories are skipped. The repository
/// itself may no longer exist — existence is the caller's decision.
pub fn known_repositories(home: &LevelerHome) -> Vec<PathBuf> {
    let mut repos: Vec<PathBuf> = match std::fs::read_dir(home.projects_dir()) {
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

    fn env_home(home: &str) -> leveler_core::EnvSnapshot {
        leveler_core::EnvSnapshot::new(
            [(
                std::ffi::OsString::from("LEVELER_HOME"),
                std::ffi::OsString::from(home),
            )],
            PathBuf::from("/"),
            PathBuf::from("/tmp"),
        )
    }

    #[test]
    fn defaults_config_dir_to_repo_configs() {
        let layout = Layout::resolve(PathBuf::from("/repo"), None);
        if std::env::var_os("LEVELER_CONFIG_DIR").is_none() {
            assert_eq!(layout.config_dir, PathBuf::from("/repo/configs"));
        }
    }

    #[test]
    fn runtime_state_lives_outside_the_repo_under_state_projects() {
        let layout = Layout::resolve_with_environment(
            PathBuf::from("/repo"),
            None,
            &env_home("/h/.leveler"),
        );
        let db = layout.database_path();
        assert!(db.ends_with("sessions.db"), "{db:?}");
        assert!(
            !db.starts_with("/repo"),
            "state must not be in the repo: {db:?}"
        );
        assert!(
            db.starts_with("/h/.leveler/state/projects"),
            "durable state is namespaced under state/projects/: {db:?}"
        );
    }

    #[test]
    fn memory_and_permissions_sit_beside_the_database() {
        let layout = Layout::resolve_with_environment(
            PathBuf::from("/repo"),
            None,
            &env_home("/h/.leveler"),
        );
        assert!(
            layout
                .memory_dir()
                .starts_with("/h/.leveler/state/projects")
        );
        assert!(
            layout
                .permissions_path()
                .starts_with("/h/.leveler/state/projects")
        );
        assert!(layout.permissions_path().ends_with("permissions.yaml"));
    }

    #[test]
    fn resolving_a_layout_writes_nothing_into_the_repository() {
        let repo = tempfile::tempdir().unwrap();
        let home = tempfile::tempdir().unwrap();
        let env = env_home(&home.path().display().to_string());

        let before: Vec<_> = fs::read_dir(repo.path())
            .unwrap()
            .flatten()
            .map(|e| e.file_name())
            .collect();
        let _ = Layout::resolve_with_environment(repo.path().to_path_buf(), None, &env);
        let after: Vec<_> = fs::read_dir(repo.path())
            .unwrap()
            .flatten()
            .map(|e| e.file_name())
            .collect();

        assert_eq!(
            before, after,
            "resolving a layout must not create anything inside the workspace"
        );
        assert!(
            !repo.path().join(".leveler").exists(),
            "no `.leveler` may appear in the repo"
        );
    }

    #[test]
    fn target_lock_path_lives_under_run_locks_never_in_the_workspace() {
        let lock = target_lock_path(&env_home("/home/x/.leveler"), Path::new("/repo/src/lib.rs"));
        assert_eq!(
            lock,
            PathBuf::from(format!(
                "/home/x/.leveler/run/locks/{}.lock",
                path_hash("/repo/src/lib.rs")
            ))
        );
    }

    #[test]
    fn socket_lives_in_run_sockets_keyed_by_repo_hash() {
        let layout = Layout::resolve_with_environment(
            PathBuf::from("/repo"),
            None,
            &env_home("/home/x/.leveler"),
        );
        // repo_root is canonicalized in resolve; recompute the hash from it.
        let expected = format!(
            "/home/x/.leveler/run/sockets/{}.sock",
            path_hash(&layout.repo_root.to_string_lossy())
        );
        assert_eq!(layout.socket_path(), PathBuf::from(expected));
    }

    #[test]
    fn socket_path_stays_under_sun_len_for_deep_repositories() {
        // The regression this guards: state-dir-based sockets overflowed
        // macOS's ~104-byte sun_path limit for deeply nested repos. run/sockets
        // keeps them short.
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
    fn encodes_repo_path_as_slug_plus_stable_hash() {
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
        assert_eq!(
            encoded,
            encode_repo_path(Path::new("/Users/me/app")),
            "stable"
        );
    }

    #[test]
    fn distinct_paths_that_share_a_slug_get_distinct_ids() {
        let a = encode_repo_path(Path::new("/tmp/a-b"));
        let b = encode_repo_path(Path::new("/tmp/a/b"));
        assert_ne!(a, b, "must not collide: {a} vs {b}");
        assert!(a.starts_with("-tmp-a-b-"));
        assert!(b.starts_with("-tmp-a-b-"));
    }

    #[test]
    fn known_repositories_reads_owner_markers_under_state_projects() {
        let base = std::env::temp_dir().join(format!(
            "leveler-known-repos-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let home = leveler_core::LevelerHome::from_root(base.join("h"));
        let proj = home.project_state_dir("-work-foo-deadbeefdeadbeef");
        fs::create_dir_all(&proj).unwrap();
        fs::write(proj.join(REPOSITORY_OWNER_FILE), b"/work/foo").unwrap();
        // A marker-less dir is skipped.
        fs::create_dir_all(home.project_state_dir("-ghost-0000000000000000")).unwrap();

        let repos = known_repositories(&home);
        assert_eq!(repos, vec![PathBuf::from("/work/foo")]);
        fs::remove_dir_all(&base).ok();
    }
}
