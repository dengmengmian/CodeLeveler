//! The single authority for every CodeLeveler-owned filesystem path.
//!
//! [`LevelerHome`] resolves the global home once (`$LEVELER_HOME`, else
//! `$HOME/.leveler`, else `%USERPROFILE%\.leveler`, else a process-local temp
//! dir) and derives every canonical sub-path from it. Business crates must ask
//! `LevelerHome` for a directory instead of joining onto the home root
//! themselves — that is what keeps the layout coherent and the workspace clean.
//!
//! Namespaces (lazy-created — asking for a path never makes a directory):
//!
//! ```text
//! <root>/
//! ├── config.toml                 global user config
//! ├── state/   projects/<id>/ · remote/ · web/     durable state
//! ├── run/     sockets/ · locks/ · sandboxes/       ephemeral runtime coordination
//! ├── cache/   tools/                               disposable, rebuildable
//! ├── runtimes/                                     managed execution deps (reserved)
//! └── logs/    leveler.log · crash/ · daemon/<id>.log   diagnostics only
//! ```
//!
//! Per-project *durable* state lives under `state/projects/<id>/`; the physical
//! location is global even though the ownership is project-scoped. User-authored
//! project config (`<repo>/.leveler/...`, `AGENTS.md`) is NOT owned here — it is
//! committable workspace content the runtime only reads.

use std::path::{Path, PathBuf};

use crate::EnvSnapshot;

/// The resolved global CodeLeveler home and the authority for its sub-paths.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LevelerHome {
    root: PathBuf,
}

impl LevelerHome {
    /// Resolve the home from the environment. Infallible: when no `HOME`/
    /// `USERPROFILE` is set (CI, sandboxes) it falls back to a process-local
    /// temp dir so the runtime always has a coherent, workspace-external home
    /// rather than each caller inventing its own fallback.
    pub fn resolve(env: &EnvSnapshot) -> Self {
        let root = crate::leveler_home_dir(env).unwrap_or_else(|| {
            env.temp_dir()
                .join(format!("leveler-{}", std::process::id()))
        });
        Self { root }
    }

    /// Wrap an already-known home root (tests, explicit `--home`, an injected
    /// path). Prefer [`Self::resolve`] for the normal path.
    pub fn from_root(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    /// The home root itself. Avoid joining onto this directly in business code —
    /// use a named accessor so new top-level entries stay reviewable.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Global user configuration file (`config.toml`).
    pub fn config_file(&self) -> PathBuf {
        self.root.join("config.toml")
    }

    /// User-authored global named-agent definitions: `agents/`. A sibling of
    /// `config.toml` — user-owned config, not machine state.
    pub fn agents_dir(&self) -> PathBuf {
        self.root.join("agents")
    }

    /// User-authored global skills: `skills/`. User-owned config, not state.
    pub fn skills_dir(&self) -> PathBuf {
        self.root.join("skills")
    }

    // ── state/ — durable ────────────────────────────────────────────────────

    /// Durable-state root: `state/`.
    pub fn state_dir(&self) -> PathBuf {
        self.root.join("state")
    }

    /// All per-project durable state: `state/projects/`.
    pub fn projects_dir(&self) -> PathBuf {
        self.state_dir().join("projects")
    }

    /// One project's durable state root: `state/projects/<project_id>/`.
    pub fn project_state_dir(&self, project_id: &str) -> PathBuf {
        self.projects_dir().join(project_id)
    }

    /// Remote pairing/identity/credential state: `state/remote/`.
    pub fn remote_state_dir(&self) -> PathBuf {
        self.state_dir().join("remote")
    }

    /// Web aggregation state: `state/web/`.
    pub fn web_state_dir(&self) -> PathBuf {
        self.state_dir().join("web")
    }

    /// Multi-project web registry: `state/web/projects.json`.
    pub fn web_projects_registry(&self) -> PathBuf {
        self.web_state_dir().join("projects.json")
    }

    /// Imported web attachment store: `state/web/uploads/`. Kept out of the
    /// workspace so importing a file never mutates the repository.
    pub fn web_uploads_dir(&self) -> PathBuf {
        self.web_state_dir().join("uploads")
    }

    // ── run/ — ephemeral runtime coordination ───────────────────────────────

    /// Ephemeral runtime-coordination root: `run/`.
    pub fn run_dir(&self) -> PathBuf {
        self.root.join("run")
    }

    /// Daemon Unix sockets: `run/sockets/`. Kept short (a per-repo hash, not the
    /// long project id) so paths stay under macOS `SUN_LEN`.
    pub fn sockets_dir(&self) -> PathBuf {
        self.run_dir().join("sockets")
    }

    /// Advisory workspace edit-locks: `run/locks/`.
    pub fn locks_dir(&self) -> PathBuf {
        self.run_dir().join("locks")
    }

    /// Per-command sandbox scratch: `run/sandboxes/`.
    pub fn sandboxes_dir(&self) -> PathBuf {
        self.run_dir().join("sandboxes")
    }

    // ── cache/ — disposable ─────────────────────────────────────────────────

    /// Disposable-cache root: `cache/`.
    pub fn cache_dir(&self) -> PathBuf {
        self.root.join("cache")
    }

    /// Disposable tool/build caches: `cache/tools/`.
    pub fn tool_cache_dir(&self) -> PathBuf {
        self.cache_dir().join("tools")
    }

    // ── runtimes/ — managed execution dependencies (reserved) ───────────────

    /// Managed execution-dependency root (reserved): `runtimes/`.
    pub fn runtimes_dir(&self) -> PathBuf {
        self.root.join("runtimes")
    }

    // ── logs/ — diagnostics only ────────────────────────────────────────────

    /// Diagnostics root: `logs/`.
    pub fn logs_dir(&self) -> PathBuf {
        self.root.join("logs")
    }

    /// Main append log: `logs/leveler.log`.
    pub fn main_log(&self) -> PathBuf {
        self.logs_dir().join("leveler.log")
    }

    /// Panic/crash records: `logs/crash/`.
    pub fn crash_dir(&self) -> PathBuf {
        self.logs_dir().join("crash")
    }

    /// Per-project daemon logs: `logs/daemon/`.
    pub fn daemon_logs_dir(&self) -> PathBuf {
        self.logs_dir().join("daemon")
    }

    /// One project's daemon log: `logs/daemon/<project_id>.log`.
    pub fn daemon_log(&self, project_id: &str) -> PathBuf {
        self.daemon_logs_dir().join(format!("{project_id}.log"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::OsString;

    fn home_at(root: &str) -> LevelerHome {
        LevelerHome::from_root(PathBuf::from(root))
    }

    #[test]
    fn every_path_is_under_root_and_in_a_canonical_namespace() {
        let h = home_at("/home/u/.leveler");
        assert_eq!(
            h.config_file(),
            PathBuf::from("/home/u/.leveler/config.toml")
        );
        assert_eq!(
            h.project_state_dir("proj-abc"),
            PathBuf::from("/home/u/.leveler/state/projects/proj-abc")
        );
        assert_eq!(
            h.web_projects_registry(),
            PathBuf::from("/home/u/.leveler/state/web/projects.json")
        );
        assert_eq!(
            h.remote_state_dir(),
            PathBuf::from("/home/u/.leveler/state/remote")
        );
        assert_eq!(
            h.sockets_dir(),
            PathBuf::from("/home/u/.leveler/run/sockets")
        );
        assert_eq!(h.locks_dir(), PathBuf::from("/home/u/.leveler/run/locks"));
        assert_eq!(
            h.sandboxes_dir(),
            PathBuf::from("/home/u/.leveler/run/sandboxes")
        );
        assert_eq!(
            h.tool_cache_dir(),
            PathBuf::from("/home/u/.leveler/cache/tools")
        );
        assert_eq!(h.runtimes_dir(), PathBuf::from("/home/u/.leveler/runtimes"));
        assert_eq!(
            h.daemon_log("proj-abc"),
            PathBuf::from("/home/u/.leveler/logs/daemon/proj-abc.log")
        );
        // Every canonical entry is one of the six top-level namespaces.
        for p in [
            h.config_file(),
            h.state_dir(),
            h.run_dir(),
            h.cache_dir(),
            h.runtimes_dir(),
            h.logs_dir(),
        ] {
            assert!(p.starts_with(h.root()));
        }
    }

    #[test]
    fn resolve_honours_leveler_home_over_everything() {
        let env = EnvSnapshot::new(
            [
                (
                    OsString::from("LEVELER_HOME"),
                    OsString::from("/custom/home"),
                ),
                (OsString::from("HOME"), OsString::from("/home/u")),
            ],
            /*current_dir*/ PathBuf::new(),
            /*temp_dir*/ PathBuf::from("/tmp"),
        );
        assert_eq!(LevelerHome::resolve(&env).root(), Path::new("/custom/home"));
    }

    #[test]
    fn resolve_falls_back_to_home_dotleveler_then_temp() {
        let env = EnvSnapshot::new(
            [(OsString::from("HOME"), OsString::from("/home/u"))],
            /*current_dir*/ PathBuf::new(),
            /*temp_dir*/ PathBuf::from("/tmp"),
        );
        assert_eq!(
            LevelerHome::resolve(&env).root(),
            Path::new("/home/u/.leveler")
        );

        // No HOME/USERPROFILE → a workspace-external temp home, never None and
        // never the cwd.
        let bare = EnvSnapshot::new(
            [],
            /*current_dir*/ PathBuf::new(),
            /*temp_dir*/ PathBuf::from("/tmp"),
        );
        let root = LevelerHome::resolve(&bare).root().to_path_buf();
        assert_eq!(root.parent(), Some(Path::new("/tmp")));
        assert!(
            root.file_name()
                .unwrap()
                .to_string_lossy()
                .starts_with("leveler-"),
            "temp fallback should be /tmp/leveler-<pid>, got {root:?}"
        );
    }

    /// The layout is lazy: asking for any path must not create it (callers
    /// create on write). A pristine root stays empty after touching every
    /// accessor.
    #[test]
    fn accessors_never_create_directories() {
        let root = std::env::temp_dir().join(format!("leveler-home-lazy-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let h = LevelerHome::from_root(&root);

        // Touch one accessor from every namespace.
        let _ = [
            h.config_file(),
            h.agents_dir(),
            h.skills_dir(),
            h.state_dir(),
            h.projects_dir(),
            h.project_state_dir("p"),
            h.remote_state_dir(),
            h.web_state_dir(),
            h.web_projects_registry(),
            h.web_uploads_dir(),
            h.run_dir(),
            h.sockets_dir(),
            h.locks_dir(),
            h.sandboxes_dir(),
            h.cache_dir(),
            h.tool_cache_dir(),
            h.runtimes_dir(),
            h.logs_dir(),
            h.main_log(),
            h.crash_dir(),
            h.daemon_logs_dir(),
            h.daemon_log("p"),
        ];

        assert!(
            !root.exists(),
            "resolving paths must not create the home tree"
        );
    }

    /// Architecture tripwire: home paths must be built through [`LevelerHome`],
    /// never re-derived ad hoc. These idioms are how that decays, so all are
    /// banned in `crates/*/src` (the two authority files — this one and
    /// `environment.rs`, which define resolution — excepted):
    ///
    /// - `from(".leveler")` — a cwd-relative fallback that writes runtime state
    ///   into whatever directory the process launched from (the exact workspace
    ///   pollution this module exists to prevent).
    /// - `|home| home.join(` — joining a fixed sub-path onto a bare home root
    ///   instead of asking `LevelerHome` for a named accessor.
    /// - `leveler_home_dir(` — the bare snapshot resolver. Business code must
    ///   go through `LevelerHome::resolve`; the sanctioned bridge for
    ///   `Option`-returning callers is `leveler_home_dir_from(..).map(|root|
    ///   LevelerHome::from_root(root).<accessor>())`, which is not this needle.
    /// - `"tool-cache"` — the pre-canonical cache dir; the tool cache is
    ///   `LevelerHome::tool_cache_dir()` (`cache/tools`).
    ///
    /// A new home sub-path should get a `LevelerHome` accessor, not a raw join.
    #[test]
    fn no_crate_rebuilds_home_paths_by_hand() {
        let crates = Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .expect("crates/leveler-core has a parent");
        let manifest = Path::new(env!("CARGO_MANIFEST_DIR"));
        // The path-resolution authority: these two files legitimately name the
        // raw resolver and the layout literals.
        let exempt = [
            manifest.join("src/home.rs"),
            manifest.join("src/environment.rs"),
        ];

        const BANNED: [(&str, &str); 4] = [
            (
                "from(\".leveler\")",
                "cwd-relative `.leveler` fallback — resolve via LevelerHome::resolve",
            ),
            (
                "|home| home.join(",
                "raw join onto the home root — add/use a LevelerHome accessor",
            ),
            (
                "leveler_home_dir(",
                "bare home resolver — go through LevelerHome::resolve (or the \
                 leveler_home_dir_from(..)→LevelerHome::from_root bridge)",
            ),
            (
                "\"tool-cache\"",
                "pre-canonical cache dir — use LevelerHome::tool_cache_dir()",
            ),
        ];

        let mut offenders = Vec::new();
        let mut stack = vec![crates.to_path_buf()];
        while let Some(dir) = stack.pop() {
            let Ok(entries) = std::fs::read_dir(&dir) else {
                continue;
            };
            for entry in entries.flatten() {
                let path = entry.path();
                if path.is_dir() {
                    // Only scan crate source trees, and skip build artifacts.
                    if path.file_name().is_some_and(|n| n == "target") {
                        continue;
                    }
                    stack.push(path);
                    continue;
                }
                if path.extension().is_none_or(|e| e != "rs") || exempt.contains(&path) {
                    continue;
                }
                // Only production source under a `src/` dir; test trees may name
                // these idioms as fixtures.
                if !path.components().any(|c| c.as_os_str() == "src") {
                    continue;
                }
                let Ok(text) = std::fs::read_to_string(&path) else {
                    continue;
                };
                for (needle, why) in BANNED {
                    if text.contains(needle) {
                        offenders.push(format!("{}: {needle} — {why}", path.display()));
                    }
                }
            }
        }
        assert!(
            offenders.is_empty(),
            "home paths must go through LevelerHome:\n{}",
            offenders.join("\n")
        );
    }
}
