//! Project registry + daemon lifecycle for the multi-project WebUI.
//!
//! The aggregation server keeps a registry of repositories the user opened
//! (`<home>/state/web/projects.json`) and brings each one online behind the
//! [`RouterService`]: probe the repo's per-daemon Unix socket first and attach
//! to a live daemon when one exists (e.g. the user's own `leveler tui`),
//! otherwise spawn `leveler --repo <path> serve --ready-json <file>` and
//! connect once the readiness file appears.
//!
//! # Stopping a project's runtime
//!
//! Two facts are kept apart on purpose. This manager owns the CHILD HANDLE of
//! a daemon it launched; it does not own the runtime's work. So a stop always
//! goes through [`ProjectManager::retire_owned`], which asks the runtime
//! (`try_retire_if_idle`) and only releases a child after admission is closed.
//! Unsupported or unanswered retirement leaves the child with its original
//! owner; a health snapshot cannot authorize replacement.
//!
//! Consequences, all deliberate:
//!
//! - restarting or removing a project whose runtime is mid-turn is REFUSED and
//!   reported, never enforced;
//! - closing the Web server does not kill a runtime that still owes work (the
//!   daemon is a long-lived owner with its own idle eviction, exactly like one
//!   started by the terminal), and
//! - a daemon this manager merely attached to is never signalled at all.
//!
//! After a hard kill of the web process the orphaned daemon still owns its Unix
//! socket, so the next web start reattaches to it instead of double-starting.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use serde::{Deserialize, Serialize};
use tokio::sync::{broadcast, oneshot};

use leveler_client_protocol::ProjectStatus;
use leveler_local_transport::LocalRuntimeService;
use leveler_project::Layout;
use leveler_runtime_host::{
    EnsureOwnedError, OwnedRuntimeError, OwnedRuntimeLaunch, ensure_owned_runtime,
    reconcile_runtime_generation,
};

use crate::router::RouterService;

/// One row of `GET /api/projects`.
#[derive(Debug, Clone, Serialize)]
pub struct ProjectInfo {
    /// Canonical repository path — the project's identity everywhere.
    pub path: String,
    /// Display name: the user-set alias when one exists, else the path's
    /// last component.
    pub name: String,
    pub status: ProjectStatus,
    /// Sessions currently listed for this project (from the router's cache).
    pub sessions: usize,
}

/// A project operation failed in a way the frontend should display.
///
/// `state` is present exactly when the failure is a RUNTIME LIFECYCLE fact
/// rather than an ordinary validation error (a bad path, an unregistered
/// project). It carries the same vocabulary the terminal and the Desktop bridge
/// use, so three shells cannot describe one runtime three ways — and so the
/// browser can offer the right next step instead of a generic error.
#[derive(Debug, thiserror::Error)]
#[error("{message}")]
pub struct ProjectError {
    pub message: String,
    pub state: Option<leveler_runtime_host::RuntimeLifecycleState>,
}

impl ProjectError {
    /// An ordinary failure: bad input, unknown project, unwritable registry.
    pub fn invalid(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            state: None,
        }
    }

    /// A runtime lifecycle failure, named in the shared vocabulary.
    pub fn lifecycle(
        state: leveler_runtime_host::RuntimeLifecycleState,
        message: impl Into<String>,
    ) -> Self {
        Self {
            message: message.into(),
            state: Some(state),
        }
    }

    /// The lifecycle state's name, for the HTTP body.
    pub fn state_name(&self) -> Option<&'static str> {
        self.state.map(|state| state.as_str())
    }
}

/// On-disk registry: the opened repository paths plus user-set display
/// aliases. Statuses and pids are runtime facts — a daemon that survived a
/// web restart is rediscovered by probing its Unix socket, not by trusting a
/// stale pid.
#[derive(Debug, Default, Serialize, Deserialize)]
struct Registry {
    projects: Vec<PathBuf>,
    /// Display aliases set via rename; absent paths fall back to the
    /// path-derived short name. `default` keeps older files (paths only)
    /// readable.
    #[serde(default)]
    aliases: HashMap<PathBuf, String>,
    /// Removed by the user: historical discovery must not re-list these even
    /// though their state dirs under `<home>/state/projects` still exist. An
    /// explicit open clears the entry.
    #[serde(default)]
    ignored: Vec<PathBuf>,
}

/// Runtime state of one registered project.
struct Entry {
    status: ProjectStatus,
    /// Signals the monitor task (which owns the spawned [`tokio::process::Child`])
    /// to stop it. `None` when the daemon was attached, not spawned — someone
    /// else's process is never signalled from here.
    ///
    /// This is NOT the authority on stopping a runtime: it is only the handle
    /// to the child this process launched. Whether the runtime MAY be stopped
    /// is answered by the runtime itself (see [`ProjectManager::retire_owned`]).
    kill: Option<oneshot::Sender<()>>,
    /// The live client for this project's runtime.
    ///
    /// Kept so the manager can ASK the runtime before acting. A project
    /// manager that could only kill would have to guess whether work is owed,
    /// and a guess is exactly how a Web restart destroys somebody's turn.
    service: Option<Arc<dyn LocalRuntimeService>>,
    /// Listed by historical discovery, not opened by the user. Discovered
    /// entries are ephemeral: excluded from the persisted registry (discovery
    /// re-runs every start) and promoted to persistent when the user opens
    /// them explicitly.
    discovered: bool,
}

/// Shared with monitor tasks so they never own the manager itself.
struct State {
    entries: Mutex<HashMap<PathBuf, Entry>>,
    /// Display aliases (loaded from / persisted with the registry).
    aliases: Mutex<HashMap<PathBuf, String>>,
    /// User-removed repositories discovery must skip (persisted).
    ignored: Mutex<std::collections::HashSet<PathBuf>>,
    /// Live status changes, fanned out to WS connections as `project_status`.
    status_tx: broadcast::Sender<(String, ProjectStatus)>,
}

impl State {
    fn set_status(&self, repo: &Path, status: ProjectStatus) {
        if let Some(entry) = self.entries.lock().unwrap().get_mut(repo) {
            entry.status = status;
        }
        let _ = self.status_tx.send((repo.display().to_string(), status));
    }
}

/// Registry + daemon lifecycle. One per aggregation server.
pub struct ProjectManager {
    router: Arc<RouterService>,
    /// The global home root, kept so historical discovery scans the real
    /// `state/projects/` rather than guessing it from the registry path.
    home_root: PathBuf,
    registry_path: PathBuf,
    /// Binary to spawn for projects with no live daemon (`current_exe`).
    /// `None` (tests, exotic setups) disables spawning; probing still works.
    exe: Option<PathBuf>,
    /// Maps a repository to its daemon Unix socket. Injectable so tests can
    /// point probes at a temp socket without touching `LEVELER_HOME`.
    socket_for: Box<dyn Fn(&Path) -> PathBuf + Send + Sync>,
    state: Arc<State>,
}

impl ProjectManager {
    pub fn new(
        router: Arc<RouterService>,
        home: leveler_core::LevelerHome,
        exe: Option<PathBuf>,
    ) -> Arc<Self> {
        Self::with_socket_resolver(router, home, exe, |repo| {
            Layout::resolve(repo.to_path_buf(), None).socket_path()
        })
    }

    pub fn with_socket_resolver(
        router: Arc<RouterService>,
        home: leveler_core::LevelerHome,
        exe: Option<PathBuf>,
        socket_for: impl Fn(&Path) -> PathBuf + Send + Sync + 'static,
    ) -> Arc<Self> {
        let registry_path = home.web_projects_registry();
        Arc::new(Self {
            router,
            home_root: home.root().to_path_buf(),
            registry_path,
            exe,
            socket_for: Box::new(socket_for),
            state: Arc::new(State {
                entries: Mutex::new(HashMap::new()),
                aliases: Mutex::new(HashMap::new()),
                ignored: Mutex::new(std::collections::HashSet::new()),
                status_tx: broadcast::channel(64).0,
            }),
        })
    }

    /// Live `(path, status)` changes for the WS layer's `project_status` frames.
    pub fn subscribe_status(&self) -> broadcast::Receiver<(String, ProjectStatus)> {
        self.state.status_tx.subscribe()
    }

    /// The primary first, then every registered project sorted by path.
    pub fn list(&self) -> Vec<ProjectInfo> {
        let primary = self.router.primary_repo().to_path_buf();
        let mut projects = vec![ProjectInfo {
            path: primary.display().to_string(),
            name: self.name_for(&primary),
            status: ProjectStatus::Online,
            sessions: self.router.session_count_for(&primary),
        }];
        let entries = self.state.entries.lock().unwrap();
        let mut registered: Vec<(&PathBuf, &Entry)> = entries.iter().collect();
        registered.sort_by_key(|(path, _)| (*path).clone());
        projects.extend(registered.into_iter().map(|(path, entry)| ProjectInfo {
            path: path.display().to_string(),
            name: self.name_for(path),
            status: entry.status,
            sessions: self.router.session_count_for(path),
        }));
        projects
    }

    /// Open a project: validate, register, bring its daemon online, persist.
    /// Idempotent — re-adding the primary or an already-registered project
    /// answers with its current state. Opening a merely-discovered project
    /// promotes it to a persistent, online one.
    pub async fn add(&self, path: &str) -> Result<ProjectInfo, ProjectError> {
        let repo = PathBuf::from(path);
        if !repo.is_dir() {
            return Err(ProjectError::invalid(format!("不是目录：{path}")));
        }
        let repo = repo.canonicalize().unwrap_or(repo);
        if repo == self.router.primary_repo() {
            return Ok(self.info_for(&repo, ProjectStatus::Online));
        } // Explicit open overrides an earlier removal.
        self.state.ignored.lock().unwrap().remove(&repo);
        {
            let mut entries = self.state.entries.lock().unwrap();
            if let Some(entry) = entries.get_mut(&repo) {
                if !entry.discovered {
                    let status = entry.status;
                    drop(entries);
                    return Ok(self.info_for(&repo, status));
                }
                // Explicit open of a discovered project: it becomes a real
                // registry member and goes through bring_online below (an
                // Online discovered entry keeps its attached daemon).
                entry.discovered = false;
                if entry.status == ProjectStatus::Online {
                    drop(entries);
                    self.persist();
                    return Ok(self.info_for(&repo, ProjectStatus::Online));
                }
            } else {
                entries.insert(
                    repo.clone(),
                    Entry {
                        status: ProjectStatus::Starting,
                        kill: None,
                        service: None,
                        discovered: false,
                    },
                );
            }
        }
        self.state.set_status(&repo, ProjectStatus::Starting);
        self.persist();
        match self.bring_online(&repo).await {
            Ok(()) => {
                self.state.set_status(&repo, ProjectStatus::Online);
                Ok(self.info_for(&repo, ProjectStatus::Online))
            }
            Err(error) => {
                // Stays registered (and persisted) as offline: the restart
                // button is the retry path, and the failure is visible.
                self.state.set_status(&repo, ProjectStatus::Offline);
                Err(error)
            }
        }
    }

    /// Unregister a project: drop its sessions from the merged list, persist,
    /// and stop the daemon ONLY if the runtime itself says it is idle.
    ///
    /// Removing a project is a UI action about a LISTING. It is not a verdict
    /// on work: a daemon that is mid-turn keeps running (it belongs to the
    /// people using it, and it reclaims itself through its own idle eviction),
    /// and an attached daemon this manager never spawned is not signalled at
    /// all. Removal must never be the thing that destroys a task.
    pub async fn remove(&self, path: &str) -> Result<(), ProjectError> {
        let repo = canonical(path);
        let entry = self.state.entries.lock().unwrap().remove(&repo);
        let Some(entry) = entry else {
            return Err(ProjectError::invalid(format!("未注册的项目：{path}")));
        };
        let mut entry = entry;
        match self.retire_owned(&mut entry).await {
            OwnedRetirement::Retired => {}
            OwnedRetirement::Deferred {
                active_turns,
                active_background_tasks,
            } => tracing::info!(
                repo = %repo.display(),
                active_turns,
                active_background_tasks,
                "project removed from the list while its runtime still owes work; the runtime keeps running and reclaims itself when idle"
            ),
            OwnedRetirement::Unavailable { reason } => tracing::info!(
                repo = %repo.display(),
                %reason,
                "project removed from the list; its runtime could not be asked to stop, so it was left alone"
            ),
        }
        self.state.aliases.lock().unwrap().remove(&repo);
        // Removal must stick across discovery passes and server restarts:
        // the repo's state dir still exists, so without this every start
        // would re-list what the user just removed.
        self.state.ignored.lock().unwrap().insert(repo.clone());
        self.router.remove_daemon(&repo);
        self.persist();
        Ok(())
    }

    /// Rename a project: set a display alias, or clear it (empty `name`) to
    /// fall back to the path-derived short name. Works for the primary too;
    /// the alias persists with the registry.
    pub fn rename(&self, path: &str, name: &str) -> Result<(), ProjectError> {
        let repo = canonical(path);
        let registered = self.state.entries.lock().unwrap().contains_key(&repo);
        if !registered && repo != self.router.primary_repo() {
            return Err(ProjectError::invalid(format!("未注册的项目：{path}")));
        }
        let name = name.trim();
        {
            let mut aliases = self.state.aliases.lock().unwrap();
            if name.is_empty() {
                aliases.remove(&repo);
            } else {
                aliases.insert(repo, name.to_string());
            }
        }
        self.persist();
        Ok(())
    }

    /// Restart a project's daemon — but only when the runtime itself says it
    /// is idle.
    ///
    /// The previous implementation signalled the child unconditionally, so
    /// pressing "restart" in the Web UI destroyed whatever the runtime was
    /// doing. Restarting a runtime is not an authorization to cancel work: the
    /// runtime is asked, and a busy answer is reported instead of enforced.
    pub async fn restart(&self, path: &str) -> Result<(), ProjectError> {
        let repo = canonical(path);
        let mut entry = {
            let mut entries = self.state.entries.lock().unwrap();
            let Some(entry) = entries.remove(&repo) else {
                return Err(ProjectError::invalid(format!("未注册的项目：{path}")));
            };
            entry
        };
        match self.retire_owned(&mut entry).await {
            OwnedRetirement::Retired => {}
            refusal => {
                // Nothing was stopped and nothing is wrong with the runtime:
                // put it back exactly as it was and keep the project Online so
                // the UI keeps it usable.
                self.state
                    .entries
                    .lock()
                    .unwrap()
                    .insert(repo.clone(), entry);
                self.state.set_status(&repo, ProjectStatus::Online);
                return Err(ProjectError::lifecycle(
                    refusal.state(),
                    refusal.refusal_message(),
                ));
            }
        }
        self.router.remove_daemon(&repo);
        self.state.set_status(&repo, ProjectStatus::Starting);
        match self.bring_online(&repo).await {
            Ok(()) => {
                self.state.set_status(&repo, ProjectStatus::Online);
                Ok(())
            }
            Err(error) => {
                self.state.set_status(&repo, ProjectStatus::Offline);
                Err(error)
            }
        }
    }

    /// Bring every registered project from the on-disk registry online.
    /// Called once at server start; failures leave the entry offline (the UI
    /// shows the restart button) instead of failing the whole server.
    pub async fn load_registry(self: Arc<Self>) {
        let registry = match std::fs::read(&self.registry_path) {
            Ok(bytes) => match serde_json::from_slice::<Registry>(&bytes) {
                Ok(registry) => registry,
                Err(error) => {
                    tracing::warn!(%error, path = %self.registry_path.display(), "unreadable project registry; starting empty");
                    return;
                }
            },
            Err(_) => return, // no registry yet
        };
        *self.state.aliases.lock().unwrap() = registry.aliases;
        *self.state.ignored.lock().unwrap() = registry.ignored.into_iter().collect();
        for repo in registry.projects {
            let Some(repo_str) = repo.to_str() else {
                continue;
            };
            if let Err(error) = self.add(repo_str).await {
                tracing::warn!(%error, repo = %repo.display(), "failed to bring a registered project online");
            }
        }
    }

    /// Discover repositories that have Leveler state under the Leveler home
    /// (derived from the registry path's parent) and list each one. Runs
    /// after [`load_registry`](Self::load_registry) at server start; this is
    /// what lets the sidebar list projects the user only ever drove from the
    /// TUI. Discovered projects are probe-or-offline and never persisted:
    /// attaching to a live daemon is cheap, but spawning one per historical
    /// repo would be a process storm — the user opening the project (or the
    /// restart button) is the spawn path.
    ///
    /// `ephemeral_root` (the OS temp dir in production) filters out throwaway
    /// checkouts: eval and fixture repos live under temp and would otherwise
    /// bury the real projects in noise. Explicitly opening such a path still
    /// works — only discovery skips it.
    pub async fn discover_historical_projects(&self, ephemeral_root: &Path) {
        for repo in historical_repositories(&self.home_root, ephemeral_root).await {
            self.register_discovered(repo).await;
        }
    }

    /// List one discovered repository: probe its daemon socket and attach when
    /// live, else show it offline. Never spawns, never persists; repositories
    /// that no longer exist (deleted checkouts, temp eval dirs) are skipped.
    async fn register_discovered(&self, repo: PathBuf) {
        if !repo.is_dir() {
            return;
        }
        let repo = repo.canonicalize().unwrap_or(repo);
        if repo == self.router.primary_repo()
            || self.state.entries.lock().unwrap().contains_key(&repo)
            || self.state.ignored.lock().unwrap().contains(&repo)
        {
            return;
        }
        self.state.entries.lock().unwrap().insert(
            repo.clone(),
            Entry {
                status: ProjectStatus::Starting,
                kill: None,
                service: None,
                discovered: true,
            },
        );
        let socket = (self.socket_for)(&repo);
        let ui = leveler_runtime_host::NonInteractiveHandoffUi::new();
        // Discovery attaches to a runtime ONLY when it is this generation's.
        // A discovered project is a repository that has CodeLeveler state, so a
        // runtime answering its endpoint is either ours (attach) or a leftover
        // from another build (report, never adopt): adopting it would serve the
        // user's next request through a runtime this build does not match.
        let status = match reconcile_runtime_generation(
            &socket,
            &Layout::resolve(repo.clone(), None),
            ui.as_ref(),
        )
        .await
        {
            Ok(Some(client)) => {
                let service: Arc<dyn LocalRuntimeService> = Arc::new(client);
                self.router.add_daemon(repo.clone(), service.clone()).await;
                if let Some(entry) = self.state.entries.lock().unwrap().get_mut(&repo) {
                    entry.service = Some(service);
                }
                ProjectStatus::Online
            }
            Ok(None) => ProjectStatus::Offline,
            Err(error) => {
                tracing::warn!(
                    repo = %repo.display(),
                    %error,
                    "a discovered project's runtime is not usable at this generation"
                );
                ProjectStatus::Offline
            }
        };
        self.state.set_status(&repo, status);
    }

    /// Probe the repo's daemon socket; attach when live, else spawn a fresh
    /// daemon and connect once it reports ready.
    async fn bring_online(&self, repo: &Path) -> Result<(), ProjectError> {
        let socket = (self.socket_for)(repo);
        let ready_path = self.exe.as_ref().map(|_| {
            std::env::temp_dir().join(format!(
                "leveler-web-ready-{}-{}.json",
                std::process::id(),
                path_nonce(repo)
            ))
        });
        let launch = self
            .exe
            .as_ref()
            .zip(ready_path.as_ref())
            .map(|(exe, ready_path)| OwnedRuntimeLaunch {
                executable: exe,
                ready_path,
            });
        let owned = ensure_owned_runtime(
            repo,
            &Layout::resolve(repo.to_path_buf(), None),
            &socket,
            launch,
            leveler_runtime_host::NonInteractiveHandoffUi::new(),
        )
        .await
        .map_err(|error| {
            let message = match &error {
                EnsureOwnedError::NoLauncher { socket } => format!(
                    "项目没有运行中的 daemon（{}），且当前环境无法代为启动",
                    socket.display()
                ),
                EnsureOwnedError::Spawn(error) => format!("启动 daemon 失败：{error}"),
                EnsureOwnedError::Ready(error) => match error {
                    OwnedRuntimeError::InvalidReadyJson(error) => {
                        format!("无法解析 daemon 就绪信息：{error}")
                    }
                    OwnedRuntimeError::MissingSocket => "daemon 就绪信息缺少 socket 字段".into(),
                    OwnedRuntimeError::ChildExited(status) => {
                        format!("daemon 启动即退出（{status}）——多半是该仓库已有 daemon 或配置错误")
                    }
                    OwnedRuntimeError::Timeout => "等待 daemon 就绪超时".into(),
                },
                EnsureOwnedError::Connect(error) => {
                    format!("daemon 已就绪但连接失败：{error}")
                }
                EnsureOwnedError::Generation { reason, .. } => {
                    format!("该项目运行时不是当前代际，且无法自动切换：{reason}")
                }
            };
            let state = error.lifecycle_state();
            ProjectError::lifecycle(state, message)
        })?;
        let service: Arc<dyn LocalRuntimeService> = Arc::new(owned.client);
        self.router
            .add_daemon(repo.to_path_buf(), service.clone())
            .await;
        if let Some(entry) = self.state.entries.lock().unwrap().get_mut(repo) {
            entry.service = Some(service);
        }

        let Some(child) = owned.child else {
            return Ok(());
        };
        // The monitor owns the child: it reaps a natural death as `offline`
        // and answers the manager's kill signal (remove / restart / shutdown).
        let (kill_tx, kill_rx) = oneshot::channel();
        if let Some(entry) = self.state.entries.lock().unwrap().get_mut(repo) {
            entry.kill = Some(kill_tx);
        }
        tokio::spawn(monitor_child(
            child,
            kill_rx,
            self.state.clone(),
            repo.to_path_buf(),
        ));
        Ok(())
    }

    fn info_for(&self, repo: &Path, status: ProjectStatus) -> ProjectInfo {
        ProjectInfo {
            path: repo.display().to_string(),
            name: self.name_for(repo),
            status,
            sessions: self.router.session_count_for(repo),
        }
    }

    /// The display name: a user-set alias when one exists, else the
    /// path-derived short name.
    fn name_for(&self, repo: &Path) -> String {
        self.state
            .aliases
            .lock()
            .unwrap()
            .get(repo)
            .cloned()
            .unwrap_or_else(|| short_name(repo))
    }

    /// Write the registry (paths + aliases). Failures are logged, not fatal —
    /// the running state is unaffected.
    fn persist(&self) {
        let registry = Registry {
            // Only user-opened projects: discovered entries are re-listed by
            // every start's discovery pass, and persisting them would make
            // the next start spawn a daemon per historical repo.
            projects: {
                let mut paths: Vec<PathBuf> = self
                    .state
                    .entries
                    .lock()
                    .unwrap()
                    .iter()
                    .filter(|(_, entry)| !entry.discovered)
                    .map(|(path, _)| path.clone())
                    .collect();
                paths.sort();
                paths
            },
            aliases: self.state.aliases.lock().unwrap().clone(),
            ignored: {
                let mut paths: Vec<PathBuf> =
                    self.state.ignored.lock().unwrap().iter().cloned().collect();
                paths.sort();
                paths
            },
        };
        let write = || -> std::io::Result<()> {
            if let Some(parent) = self.registry_path.parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::write(&self.registry_path, serde_json::to_vec_pretty(&registry)?)
        };
        if let Err(error) = write() {
            tracing::warn!(%error, path = %self.registry_path.display(), "failed to persist the project registry");
        }
    }
}

/// Every repository with Leveler state under `<home>/projects/*`: the
/// `.repository-root` ownership marker where present, else the repository
/// recorded in the dir's `sessions.db` (state dirs created before the marker
/// existed — exactly the TUI-only history the sidebar must still list).
/// Repositories under `ephemeral_root` are dropped as throwaway checkouts;
/// vanished repositories are the caller's filter.
async fn historical_repositories(home: &Path, ephemeral_root: &Path) -> Vec<PathBuf> {
    let ephemeral = ephemeral_root
        .canonicalize()
        .unwrap_or_else(|_| ephemeral_root.to_path_buf());
    let home = leveler_core::LevelerHome::from_root(home);
    let mut repos = leveler_project::layout::known_repositories(&home);
    if let Ok(entries) = std::fs::read_dir(home.projects_dir()) {
        for entry in entries.filter_map(Result::ok) {
            let dir = entry.path();
            if dir
                .join(leveler_project::layout::REPOSITORY_OWNER_FILE)
                .exists()
            {
                continue; // already covered by the marker pass
            }
            if let Some(repo) = leveler_storage::peek_repository(&dir.join("sessions.db")).await {
                let repo = PathBuf::from(repo);
                if !repos.contains(&repo) {
                    repos.push(repo);
                }
            }
        }
    }
    repos.retain(|repo| {
        // Canonicalize so the macOS `/var` → `/private/var` symlink cannot
        // dodge the prefix check.
        let canonical = repo.canonicalize().unwrap_or_else(|_| repo.clone());
        !canonical.starts_with(&ephemeral)
    });
    repos.sort();
    repos
}

/// Own the spawned child until it dies or the manager asks for its death.
async fn monitor_child(
    mut child: tokio::process::Child,
    kill: oneshot::Receiver<()>,
    state: Arc<State>,
    repo: PathBuf,
) {
    tokio::select! {
        status = child.wait() => {
            tracing::warn!(repo = %repo.display(), ?status, "project daemon exited");
            state.set_status(&repo, ProjectStatus::Offline);
        }
        command = kill => {
            match command {
                Ok(()) => {
                    let _ = child.start_kill();
                    let _ = child.wait().await;
                }
                Err(_) => {
                    // Unregistering a project or closing the manager drops
                    // this sender. Channel closure grants no retirement
                    // authority; retain supervision until the owner exits.
                    let status = child.wait().await;
                    tracing::warn!(repo = %repo.display(), ?status, "project daemon exited");
                    state.set_status(&repo, ProjectStatus::Offline);
                }
            }
        }
    }
}

/// What happened when the manager asked an OWNED runtime to stop.
///
/// Three outcomes, because "we stopped it" and "we could not ask" are not the
/// same fact and neither is "it said no".
enum OwnedRetirement {
    /// The runtime agreed it owed nothing and has been released.
    Retired,
    /// The runtime answered that it still owes work. NOTHING was changed and
    /// nothing was signalled: it keeps admitting work and keeps serving.
    Deferred {
        active_turns: u32,
        active_background_tasks: u32,
    },
    /// No authoritative answer could be obtained, so nothing was stopped.
    /// Silence is never permission to destroy work.
    Unavailable { reason: String },
}

impl OwnedRetirement {
    /// The shared vocabulary name for this outcome.
    ///
    /// `Retired` is not a lifecycle state — the runtime is gone, so the caller
    /// starts the next generation or reports Offline. `Deferred` is exactly
    /// [`RuntimeLifecycleState::UpgradeDeferred`]: the runtime answered that it
    /// owes work and nothing was changed. `Unavailable` means the manager could
    /// not read an authoritative answer, which is [`RuntimeLifecycleState::Unresponsive`].
    fn state(&self) -> leveler_runtime_host::RuntimeLifecycleState {
        use leveler_runtime_host::RuntimeLifecycleState;
        match self {
            Self::Retired => RuntimeLifecycleState::Unresponsive,
            Self::Deferred {
                active_turns,
                active_background_tasks,
            } => RuntimeLifecycleState::UpgradeDeferred {
                active_turns: *active_turns,
                active_background_tasks: *active_background_tasks,
            },
            Self::Unavailable { .. } => RuntimeLifecycleState::Unresponsive,
        }
    }

    fn refusal_message(&self) -> String {
        match self {
            Self::Retired => String::new(),
            Self::Deferred {
                active_turns,
                active_background_tasks,
            } => format!(
                "该项目运行时仍在执行任务（轮次 {active_turns} · 后台任务 {active_background_tasks}），已拒绝重启；请先完成或停止这些任务"
            ),
            Self::Unavailable { reason } => {
                format!(
                    "无法确认该项目运行时是否空闲（{reason}），已拒绝重启；请从启动它的客户端停止它"
                )
            }
        }
    }
}

impl ProjectManager {
    /// Ask this project's runtime whether it may stop, and act on its answer.
    ///
    /// This is the ONE place a Web-managed runtime may be stopped from, and it
    /// can only be reached by the runtime's own verdict:
    ///
    /// - `Accepted` / `AlreadyRetiring` — admission is closed and nothing was
    ///   owed, so releasing the process destroys no work;
    /// - `Busy` — reported, never enforced;
    /// - `GenerationChanged` — another client is already replacing it, so this
    ///   manager steps aside instead of racing;
    /// - `Unsupported` or an unanswered request — a fresh busy report explains
    ///   a deferral; apparent idleness still cannot authorize stopping. The
    ///   runtime's original owner must exit normally.
    ///
    /// A daemon this manager only attached to is never signalled at all: there
    /// is no child handle for it, and it is not ours to end.
    async fn retire_owned(&self, entry: &mut Entry) -> OwnedRetirement {
        let Some(service) = entry.service.clone() else {
            // No live client was recorded. Only a handle to a child THIS
            // process launched can be released, and there is no runtime whose
            // state could be read, so nothing about it is guessed.
            if let Some(kill) = entry.kill.take() {
                let _ = kill.send(());
            }
            return OwnedRetirement::Retired;
        };
        let info = match service.runtime_info().await {
            Ok(info) => info,
            Err(error) => {
                return OwnedRetirement::Unavailable {
                    reason: error.to_string(),
                };
            }
        };
        let request = leveler_client_protocol::RetireRequest {
            reason: leveler_client_protocol::RestartReason::RestartRequested,
            expected_build: info.build.clone(),
            expected_pid: info.pid,
        };
        match service.try_retire_if_idle(request).await {
            Ok(leveler_client_protocol::RetireDecision::Accepted)
            | Ok(leveler_client_protocol::RetireDecision::AlreadyRetiring { .. }) => {
                if let Some(kill) = entry.kill.take() {
                    let _ = kill.send(());
                }
                OwnedRetirement::Retired
            }
            Ok(leveler_client_protocol::RetireDecision::Busy {
                active_turns,
                active_background_tasks,
            }) => OwnedRetirement::Deferred {
                active_turns,
                active_background_tasks,
            },
            Ok(leveler_client_protocol::RetireDecision::GenerationChanged) => {
                OwnedRetirement::Unavailable {
                    reason: "运行时已被另一客户端替换".to_string(),
                }
            }
            unconfirmed => {
                let reason = match unconfirmed {
                    Ok(leveler_client_protocol::RetireDecision::Unsupported) => {
                        "运行时不支持原子退休；请先让原运行时正常退出后再操作".to_string()
                    }
                    Err(error) => format!("无法确认运行时原子退休：{error}"),
                    _ => unreachable!("confirmed retirement decisions are handled above"),
                };
                // A fresh busy report can explain a deferral, but an idle
                // snapshot cannot close admission and authorize child release.
                match service.runtime_info().await {
                    Ok(fresh)
                        if fresh.health.active_turns > 0
                            || fresh.health.active_background_tasks > 0 =>
                    {
                        OwnedRetirement::Deferred {
                            active_turns: fresh.health.active_turns,
                            active_background_tasks: fresh.health.active_background_tasks,
                        }
                    }
                    Ok(_) => OwnedRetirement::Unavailable { reason },
                    Err(error) => OwnedRetirement::Unavailable {
                        reason: format!("{reason}；无法读取运行时状态：{error}"),
                    },
                }
            }
        }
    }
}

fn canonical(path: &str) -> PathBuf {
    let repo = PathBuf::from(path);
    repo.canonicalize().unwrap_or(repo)
}

fn short_name(path: &Path) -> String {
    path.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.display().to_string())
}

/// A per-repo filename nonce (same encoding idea as the state dir, shortened).
fn path_nonce(repo: &Path) -> String {
    leveler_project::layout::encode_repo_path(repo)
        .chars()
        .rev()
        .take(16)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use leveler_client_protocol::{
        ClientCommand, ClientError, InteractiveRuntimeClient, RuntimeEvent, SessionId,
        UiSessionSnapshot, mock::MockRuntimeClient,
    };
    use leveler_local_transport::{CreateSessionRequest, LocalRuntimeService, SessionBootstrap};
    // Unix-socket daemon fixture is unix-only (Windows stubs return Unavailable).
    #[cfg(unix)]
    use leveler_local_transport::{LocalSocketRuntimeClient, LocalSocketServer};

    /// Minimal primary service: the manager tests never exercise commands.
    struct StubService {
        mock: MockRuntimeClient,
        /// The generation this stub claims, so it can stand in for a runtime of
        /// the generation under test. A stub with no identity would (correctly)
        /// be refused by the generation check, which is what the daemon tests
        /// of that refusal cover.
        generation: String,
    }

    impl StubService {
        fn new() -> Arc<Self> {
            let repo = std::env::temp_dir().join("leveler-web-stub-primary");
            let layout = Layout::resolve(repo, None);
            Arc::new(Self {
                mock: MockRuntimeClient::new(SessionId::new("stub")),
                generation: leveler_runtime_host::expected_config_fingerprint(&layout)
                    .unwrap_or_default(),
            })
        }
    }

    #[async_trait]
    impl InteractiveRuntimeClient for StubService {
        async fn send(&self, command: ClientCommand) -> Result<(), ClientError> {
            self.mock.send(command).await
        }
        fn subscribe(&self) -> broadcast::Receiver<RuntimeEvent> {
            self.mock.subscribe()
        }
        async fn snapshot(&self, session_id: &SessionId) -> Result<UiSessionSnapshot, ClientError> {
            self.mock.snapshot(session_id).await
        }
    }

    #[async_trait]
    impl LocalRuntimeService for StubService {
        async fn create_session(
            &self,
            _request: CreateSessionRequest,
        ) -> Result<SessionBootstrap, ClientError> {
            Err(ClientError::Runtime("not exercised".to_string()))
        }

        async fn runtime_info(&self) -> Result<leveler_client_protocol::RuntimeInfo, ClientError> {
            Ok(leveler_client_protocol::RuntimeInfo {
                runtime_id: leveler_core::RuntimeId::new("web-stub-runtime"),
                version: env!("CARGO_PKG_VERSION").to_string(),
                build: leveler_core::BuildIdentity::current(),
                config_fingerprint: Some(self.generation.clone()),
                pid: std::process::id(),
                health: leveler_client_protocol::RuntimeHealth {
                    accepting_work: true,
                    quiescent: true,
                    ..Default::default()
                },
            })
        }
    }

    fn manager_in(dir: &Path, socket: PathBuf) -> (Arc<ProjectManager>, Arc<RouterService>) {
        let router = RouterService::new(StubService::new(), dir.join("primary"));
        let manager = ProjectManager::with_socket_resolver(
            router.clone(),
            leveler_core::LevelerHome::from_root(dir.to_path_buf()),
            None,
            move |_repo| socket.clone(),
        );
        (manager, router)
    }

    /// A daemon that is WORKING, and that records every command it receives.
    ///
    /// The whole PR3 contract is one question: did the Web layer ask, or did it
    /// kill? This service answers `Busy` to the atomic request and remembers
    /// whether anyone tried to shut it down the fire-and-forget way.
    struct WorkingService {
        commands: Arc<Mutex<Vec<String>>>,
        quiescent: bool,
        atomic_supported: bool,
        retirement_error: bool,
        pid: u32,
        /// The generation this stub claims. See `StubService::generation`.
        generation: String,
    }

    impl WorkingService {
        fn new(quiescent: bool, atomic_supported: bool) -> Arc<Self> {
            let repo = std::env::temp_dir().join("leveler-web-working-project");
            let layout = Layout::resolve(repo, None);
            Arc::new(Self {
                commands: Arc::new(Mutex::new(Vec::new())),
                quiescent,
                atomic_supported,
                retirement_error: false,
                pid: std::process::id(),
                generation: leveler_runtime_host::expected_config_fingerprint(&layout)
                    .unwrap_or_default(),
            })
        }

        fn health(&self) -> leveler_client_protocol::RuntimeHealth {
            if self.quiescent {
                leveler_client_protocol::RuntimeHealth {
                    accepting_work: true,
                    quiescent: true,
                    ..Default::default()
                }
            } else {
                leveler_client_protocol::RuntimeHealth {
                    accepting_work: true,
                    active_turns: 1,
                    quiescent: false,
                    ..Default::default()
                }
            }
        }
    }

    #[async_trait]
    impl InteractiveRuntimeClient for WorkingService {
        async fn send(&self, command: ClientCommand) -> Result<(), ClientError> {
            self.commands.lock().unwrap().push(format!("{command:?}"));
            Ok(())
        }

        fn subscribe(&self) -> broadcast::Receiver<RuntimeEvent> {
            broadcast::channel(4).1
        }

        async fn snapshot(
            &self,
            _session_id: &SessionId,
        ) -> Result<UiSessionSnapshot, ClientError> {
            Err(ClientError::Runtime("not exercised".to_string()))
        }
    }

    #[async_trait]
    impl LocalRuntimeService for WorkingService {
        async fn create_session(
            &self,
            _request: CreateSessionRequest,
        ) -> Result<SessionBootstrap, ClientError> {
            Err(ClientError::Runtime("not exercised".to_string()))
        }

        async fn runtime_info(&self) -> Result<leveler_client_protocol::RuntimeInfo, ClientError> {
            Ok(leveler_client_protocol::RuntimeInfo {
                runtime_id: leveler_core::RuntimeId::new("web-test-runtime"),
                version: env!("CARGO_PKG_VERSION").to_string(),
                build: leveler_core::BuildIdentity::current(),
                config_fingerprint: Some(self.generation.clone()),
                pid: self.pid,
                health: self.health(),
            })
        }

        async fn try_retire_if_idle(
            &self,
            _request: leveler_client_protocol::RetireRequest,
        ) -> Result<leveler_client_protocol::RetireDecision, ClientError> {
            if self.retirement_error {
                return Err(ClientError::Runtime("retirement outcome unknown".into()));
            }
            if !self.atomic_supported {
                return Ok(leveler_client_protocol::RetireDecision::Unsupported);
            }
            if self.quiescent {
                Ok(leveler_client_protocol::RetireDecision::Accepted)
            } else {
                Ok(leveler_client_protocol::RetireDecision::Busy {
                    active_turns: 1,
                    active_background_tasks: 0,
                })
            }
        }
    }

    /// Only the commands that would STOP a runtime. The router legitimately
    /// asks a newly attached daemon for its session list, so a bare "no commands"
    /// assertion would be about the wrong thing.
    #[cfg(unix)]
    fn stop_commands(commands: &Arc<Mutex<Vec<String>>>) -> Vec<String> {
        commands
            .lock()
            .unwrap()
            .iter()
            .filter(|command| {
                command.contains("ShutdownWhenIdle")
                    || command.contains("ForceRetire")
                    || command.contains("Quit")
            })
            .cloned()
            .collect()
    }

    /// Serve `service` on `socket`, so the manager reaches it through the real
    /// local transport rather than a hand-written stub.
    #[cfg(unix)]
    async fn serve_service(
        socket: &Path,
        service: Arc<dyn LocalRuntimeService>,
    ) -> tokio_util::sync::CancellationToken {
        let server = LocalSocketServer::bind(socket, service)
            .await
            .expect("test daemon binds");
        let shutdown = tokio_util::sync::CancellationToken::new();
        let serve_shutdown = shutdown.clone();
        tokio::spawn(async move {
            let _ = server.serve(serve_shutdown).await;
        });
        shutdown
    }

    /// A project whose runtime is mid-turn must be REPORTED as un-restartable,
    /// not restarted: no command is sent, nothing is killed, and the project
    /// stays usable.
    #[cfg(unix)]
    #[tokio::test]
    async fn restart_refuses_while_the_runtime_owes_work() {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("busy.sock");
        let service = WorkingService::new(false, true);
        let commands = service.commands.clone();
        let _shutdown = serve_service(&socket, service).await;
        let repo = dir.path().join("busy-project");
        std::fs::create_dir_all(&repo).unwrap();

        let (manager, router) = manager_in(dir.path(), socket);
        manager.add(repo.to_str().unwrap()).await.expect("attaches");

        let error = manager
            .restart(repo.to_str().unwrap())
            .await
            .expect_err("a busy runtime must not be restarted");
        assert!(
            error.message.contains("已拒绝重启"),
            "the refusal must be explicit: {error}"
        );
        assert!(
            stop_commands(&commands).is_empty(),
            "no stop command may be sent to a runtime that owes work: {:?}",
            stop_commands(&commands)
        );
        let canonical = repo.canonicalize().unwrap();
        assert!(
            router.handles(&canonical),
            "the runtime must stay attached after a refused restart"
        );
        assert_eq!(
            manager
                .list()
                .into_iter()
                .find(|project| project.path == canonical.display().to_string())
                .map(|project| project.status),
            Some(ProjectStatus::Online),
            "a refused restart must leave the project usable"
        );
    }

    /// Removing a project is an action about a LISTING: a runtime that owes work
    /// keeps running, and is not even asked to stop.
    #[cfg(unix)]
    #[tokio::test]
    async fn remove_does_not_stop_a_working_runtime() {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("busy-remove.sock");
        let service = WorkingService::new(false, true);
        let commands = service.commands.clone();
        let _shutdown = serve_service(&socket, service).await;
        let repo = dir.path().join("busy-remove-project");
        std::fs::create_dir_all(&repo).unwrap();

        let (manager, _router) = manager_in(dir.path(), socket);
        manager.add(repo.to_str().unwrap()).await.expect("attaches");
        manager
            .remove(repo.to_str().unwrap())
            .await
            .expect("removal succeeds");
        assert!(
            stop_commands(&commands).is_empty(),
            "removal must not send a stop command to a working runtime: {:?}",
            stop_commands(&commands)
        );
        assert!(
            manager
                .list()
                .into_iter()
                .all(|project| project.path != repo.canonicalize().unwrap().display().to_string()),
            "the project is unregistered"
        );
    }

    #[tokio::test]
    async fn unsupported_retirement_preserves_child_and_sends_no_commands() {
        assert_unconfirmed_retirement_preserves_owner(false).await;
    }

    #[tokio::test]
    async fn unanswered_retirement_preserves_child_and_sends_no_commands() {
        assert_unconfirmed_retirement_preserves_owner(true).await;
    }

    async fn assert_unconfirmed_retirement_preserves_owner(retirement_error: bool) {
        for quiescent in [false, true] {
            let dir = tempfile::tempdir().unwrap();
            let (manager, _) = manager_in(dir.path(), dir.path().join("unused.sock"));
            let mut service = WorkingService::new(quiescent, false);
            Arc::get_mut(&mut service).unwrap().retirement_error = retirement_error;
            let commands = service.commands.clone();
            let (kill, mut receiver) = oneshot::channel();
            let mut entry = Entry {
                status: ProjectStatus::Online,
                kill: Some(kill),
                service: Some(service),
                discovered: false,
            };
            let result = manager.retire_owned(&mut entry).await;
            assert!(
                commands.lock().unwrap().is_empty(),
                "no health snapshot may authorize a shutdown command"
            );
            assert!(
                entry.kill.is_some(),
                "the original owner retains its child handle"
            );
            assert!(
                matches!(
                    receiver.try_recv(),
                    Err(oneshot::error::TryRecvError::Empty)
                ),
                "no kill notification may be delivered without atomic retirement"
            );
            if quiescent {
                assert!(matches!(result, OwnedRetirement::Unavailable { .. }));
            } else {
                assert!(matches!(
                    result,
                    OwnedRetirement::Deferred {
                        active_turns: 1,
                        active_background_tasks: 0,
                    }
                ));
            }
        }
    }

    #[tokio::test]
    async fn confirmed_atomic_retirement_releases_only_the_owned_child() {
        for retire in [
            leveler_client_protocol::RetireDecision::Accepted,
            leveler_client_protocol::RetireDecision::AlreadyRetiring {
                reason: Some(leveler_client_protocol::RestartReason::RestartRequested),
            },
        ] {
            let dir = tempfile::tempdir().unwrap();
            let (manager, _) = manager_in(dir.path(), dir.path().join("unused.sock"));
            let service = ForeignBuildService::new(retire);
            let commands = service.commands.clone();
            let (kill, mut receiver) = oneshot::channel();
            let mut entry = Entry {
                status: ProjectStatus::Online,
                kill: Some(kill),
                service: Some(service),
                discovered: false,
            };
            assert!(matches!(
                manager.retire_owned(&mut entry).await,
                OwnedRetirement::Retired
            ));
            assert!(entry.kill.is_none());
            assert_eq!(receiver.try_recv(), Ok(()));
            assert!(commands.lock().unwrap().is_empty());
        }
    }

    const OWNED_HTTP_SERVICE_SCRIPT: &str = r#"import sys
print('startup: before imports',file=sys.stderr,flush=True)
import http.server,threading,json,os,socketserver
print('startup: imports complete; before HTTP bind',file=sys.stderr,flush=True)
# This numeric-loopback fixture needs a real bind, not external reverse DNS.
class OwnedHTTPServer(http.server.HTTPServer):
 def server_bind(self):
  socketserver.TCPServer.server_bind(self)
  self.server_name,self.server_port=self.server_address[:2]
class Handler(http.server.BaseHTTPRequestHandler):
 def do_GET(self):
  self.send_response(200);self.end_headers();self.wfile.write(b'owned-service-live')
  if self.path=='/exit':threading.Thread(target=self.server.shutdown).start()
 def log_message(self,*args):pass
server=OwnedHTTPServer(('127.0.0.1',0),Handler)
print('startup: HTTP bound; before READY',file=sys.stderr,flush=True)
print(json.dumps({'port':server.server_address[1],'pid':os.getpid()}),flush=True)
server.serve_forever(poll_interval=0.01)
server.server_close()
"#;

    /// A failed test startup retains evidence and reaps only its owned child.
    async fn owned_fixture_ready(
        child: &mut tokio::process::Child,
        output: &mut tokio::io::BufReader<tokio::process::ChildStdout>,
        stderr_path: &std::path::Path,
        budget: std::time::Duration,
    ) -> Result<serde_json::Value, String> {
        use tokio::io::AsyncBufReadExt;
        let pid = child.id();
        let started = tokio::time::Instant::now();
        let mut line = Vec::new();
        let mut stdout_eof = false;
        // read_until preserves partial stdout if timeout cancels the read.
        let result = async {
            let read = tokio::time::timeout(budget, output.read_until(b'\n', &mut line))
                .await
                .map_err(|_| format!("READY timeout after {budget:?}"))?
                .map_err(|error| format!("READY stdout read: {error}"))?;
            if read == 0 {
                stdout_eof = true;
                return Err("READY stdout EOF".to_owned());
            }
            let ready: serde_json::Value =
                serde_json::from_slice(&line).map_err(|error| format!("READY JSON: {error}"))?;
            if ready["pid"].as_u64() != pid.map(u64::from) {
                return Err("READY PID mismatch".to_owned());
            }
            if !matches!(ready["port"].as_u64(), Some(1..=65535)) {
                return Err("READY port invalid".to_owned());
            }
            Ok(ready)
        }
        .await;
        match result {
            Ok(ready) => Ok(ready),
            Err(cause) => {
                // EOF can precede observable process exit. Give that exit
                // only the remaining startup budget, then clean up if needed.
                let natural_exit = if stdout_eof {
                    match tokio::time::timeout(
                        budget.saturating_sub(started.elapsed()),
                        child.wait(),
                    )
                    .await
                    {
                        Ok(status) => format!("EOF wait={status:?}"),
                        Err(_) => "EOF wait timed out within startup budget".to_owned(),
                    }
                } else {
                    "no EOF wait".to_owned()
                };
                let before = child.try_wait();
                let state = match &before {
                    Ok(Some(status)) => format!("exited {status}"),
                    Ok(None) => "alive".to_owned(),
                    Err(error) => format!("try_wait failed: {error}"),
                };
                // Even on an observation error, cleanup remains confined to
                // this Child handle. kill_on_drop is only panic insurance.
                let kill = if matches!(before, Ok(Some(_))) {
                    "unnecessary".to_owned()
                } else {
                    match child.start_kill() {
                        Ok(()) => "requested".to_owned(),
                        Err(error) => format!("failed: {error}"),
                    }
                };
                let reaped = child.wait().await;
                let stderr = match std::fs::read_to_string(stderr_path) {
                    Ok(stderr) => stderr,
                    Err(error) => format!("stderr capture failed: {error}"),
                };
                Err(format!(
                    "{cause}; owned pid={pid:?}; before cleanup={state}; {natural_exit}; kill={kill}; wait={reaped:?}; stdout={:?}; stderr={stderr:?}",
                    String::from_utf8_lossy(&line),
                ))
            }
        }
    }

    #[tokio::test]
    async fn owned_fixture_startup_exit_reports_stderr_and_reaps() {
        let dir = tempfile::tempdir().unwrap();
        let stderr_path = dir.path().join("startup.stderr");
        let mut child = tokio::process::Command::new("python3")
            .args([
                "-u",
                "-c",
                "import sys;print('startup-exit-sentinel',file=sys.stderr,flush=True);sys.exit(7)",
            ])
            .stdout(std::process::Stdio::piped())
            .stderr(std::fs::File::create(&stderr_path).unwrap())
            .kill_on_drop(true)
            .spawn()
            .unwrap();
        let pid = child.id().unwrap();
        let mut output = tokio::io::BufReader::new(child.stdout.take().unwrap());
        let error = owned_fixture_ready(
            &mut child,
            &mut output,
            &stderr_path,
            std::time::Duration::from_secs(5),
        )
        .await
        .unwrap_err();
        let reaped = child.id().is_none();
        // Keep the RED test hygienic even when the old helper has not reaped.
        let status = child.wait().await.unwrap();
        assert!(error.contains("startup-exit-sentinel"), "{error}");
        assert!(error.contains("wait=Ok("), "{error}");
        assert_eq!(status.code(), Some(7));
        assert!(reaped, "failed startup must be explicitly reaped");
        assert!(!leveler_runtime_host::process_alive(pid));
    }

    #[tokio::test]
    async fn owned_fixture_startup_timeout_reports_alive_child_and_reaps() {
        use tokio::io::AsyncBufReadExt;
        let dir = tempfile::tempdir().unwrap();
        let stderr_path = dir.path().join("startup.stderr");
        let mut child = tokio::process::Command::new("python3")
            .args(["-u", "-c", "import sys,threading;print('startup-alive-sentinel',file=sys.stderr,flush=True);print('armed',flush=True);threading.Event().wait()"])
            .stdout(std::process::Stdio::piped())
            .stderr(std::fs::File::create(&stderr_path).unwrap())
            .kill_on_drop(true)
            .spawn().unwrap();
        let pid = child.id().unwrap();
        let mut output = tokio::io::BufReader::new(child.stdout.take().unwrap());
        let mut armed = String::new();
        tokio::time::timeout(
            std::time::Duration::from_secs(5),
            output.read_line(&mut armed),
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(armed, "armed\n");
        assert!(
            leveler_runtime_host::process_alive(pid),
            "owned child actually alive before timeout"
        );
        let error = owned_fixture_ready(
            &mut child,
            &mut output,
            &stderr_path,
            std::time::Duration::from_millis(100),
        )
        .await
        .unwrap_err();
        let reaped = child.id().is_none();
        if !reaped {
            child.kill().await.unwrap();
            child.wait().await.unwrap();
        }
        assert!(error.contains("startup-alive-sentinel"), "{error}");
        assert!(
            error.contains("timeout") && error.contains("alive"),
            "{error}"
        );
        assert!(
            reaped,
            "timeout must kill and explicitly reap the exact owned child"
        );
        assert!(!leveler_runtime_host::process_alive(pid));
    }

    #[tokio::test]
    async fn owned_http_fixture_does_not_depend_on_reverse_dns() {
        let dir = tempfile::tempdir().unwrap();
        let stderr_path = dir.path().join("dns-trap.stderr");
        let script = format!(
            "import socket\ndef forbidden_reverse_dns(*args,**kwargs):\n raise AssertionError('reverse-dns-trap-sentinel')\nsocket.getfqdn=forbidden_reverse_dns\n{OWNED_HTTP_SERVICE_SCRIPT}"
        );
        let mut child = tokio::process::Command::new("python3")
            .args(["-u", "-c", &script])
            .stdout(std::process::Stdio::piped())
            .stderr(std::fs::File::create(&stderr_path).unwrap())
            .kill_on_drop(true)
            .spawn()
            .unwrap();
        let pid = child.id().unwrap();
        let mut output = tokio::io::BufReader::new(child.stdout.take().unwrap());
        let ready = owned_fixture_ready(
            &mut child,
            &mut output,
            &stderr_path,
            std::time::Duration::from_secs(5),
        )
        .await
        .unwrap();
        assert_eq!(ready["pid"].as_u64(), Some(u64::from(pid)));
        let url = format!("http://127.0.0.1:{}", ready["port"]);
        let http = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(2))
            .build()
            .unwrap();
        assert_eq!(
            http.get(&url).send().await.unwrap().text().await.unwrap(),
            "owned-service-live"
        );
        http.get(format!("{url}/exit")).send().await.unwrap();
        let status = tokio::time::timeout(std::time::Duration::from_secs(5), child.wait())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(status.code(), Some(0));
        assert!(
            !leveler_runtime_host::process_alive(pid),
            "normal HTTP owner exit is reaped"
        );
    }

    #[tokio::test]
    async fn removing_unconfirmed_or_busy_runtime_preserves_real_child_until_owner_exit() {
        for (quiescent, atomic_supported, retirement_error) in [
            (true, false, false),
            (false, false, false),
            (false, true, false),
            (true, false, true),
        ] {
            let dir = tempfile::tempdir().unwrap();
            let repo = dir.path().join("owned-project");
            std::fs::create_dir(&repo).unwrap();
            let repo = repo.canonicalize().unwrap();
            let (manager, _) = manager_in(dir.path(), dir.path().join("unused.sock"));

            let stderr_path = dir.path().join("owned-service.stderr");
            let mut child = tokio::process::Command::new("python3")
                .args(["-u", "-c", OWNED_HTTP_SERVICE_SCRIPT])
                .stdout(std::process::Stdio::piped())
                .stderr(std::fs::File::create(&stderr_path).unwrap())
                .kill_on_drop(true)
                .spawn()
                .unwrap();
            let pid = child.id().unwrap();
            let mut output = tokio::io::BufReader::new(child.stdout.take().unwrap());
            let ready = owned_fixture_ready(
                &mut child,
                &mut output,
                &stderr_path,
                std::time::Duration::from_secs(5),
            )
            .await
            .unwrap();
            assert_eq!(ready["pid"].as_u64(), Some(u64::from(pid)));
            let url = format!("http://127.0.0.1:{}", ready["port"]);
            eprintln!("owned monitor fixture pid={pid} url={url}");
            let http = reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(2))
                .build()
                .unwrap();
            assert_eq!(
                http.get(&url).send().await.unwrap().text().await.unwrap(),
                "owned-service-live"
            );
            let mut service = WorkingService::new(quiescent, atomic_supported);
            let fake = Arc::get_mut(&mut service).unwrap();
            fake.retirement_error = retirement_error;
            fake.pid = pid;
            let commands = service.commands.clone();
            let (kill, receiver) = oneshot::channel();
            manager.state.entries.lock().unwrap().insert(
                repo.clone(),
                Entry {
                    status: ProjectStatus::Online,
                    kill: Some(kill),
                    service: Some(service),
                    discovered: false,
                },
            );
            let mut monitoring = tokio::spawn(monitor_child(
                child,
                receiver,
                manager.state.clone(),
                repo.clone(),
            ));
            manager.remove(repo.to_str().unwrap()).await.unwrap();
            // This timeout tests the monitor's wait contract after channel
            // closure; it is not a sleep used to synchronize admission.
            let monitor_keeps_waiting =
                tokio::time::timeout(std::time::Duration::from_millis(100), &mut monitoring)
                    .await
                    .is_err();
            let after = http.get(&url).send().await;
            let live = match after {
                Ok(response) => response.text().await.unwrap() == "owned-service-live",
                Err(_) => false,
            };
            if live {
                http.get(format!("{url}/exit")).send().await.unwrap();
                tokio::time::timeout(std::time::Duration::from_secs(5), monitoring)
                    .await
                    .unwrap()
                    .unwrap();
            }
            assert!(
                monitor_keeps_waiting,
                "sender drop is not an authorized child kill"
            );
            assert!(live, "the same owned service must survive project removal");
            assert!(
                commands.lock().unwrap().is_empty(),
                "no unconfirmed shutdown command"
            );
            assert!(
                !leveler_runtime_host::process_alive(pid),
                "owner's normal HTTP exit must be reaped"
            );
        }
    }

    /// When the runtime IS idle, the stop goes through the runtime's own
    /// atomic agreement. An older runtime's idle report cannot replace it.
    #[cfg(unix)]
    #[tokio::test]
    async fn restart_asks_an_idle_runtime_and_is_agreed_to() {
        for atomic_supported in [true, false] {
            let dir = tempfile::tempdir().unwrap();
            let socket = dir.path().join(format!("idle-{atomic_supported}.sock"));
            let service = WorkingService::new(true, atomic_supported);
            let commands = service.commands.clone();
            let _shutdown = serve_service(&socket, service).await;
            let repo = dir.path().join(format!("idle-project-{atomic_supported}"));
            std::fs::create_dir_all(&repo).unwrap();

            let (manager, _router) = manager_in(dir.path(), socket);
            manager.add(repo.to_str().unwrap()).await.expect("attaches");
            // An idle runtime accepts retirement; the manager then re-attaches
            // to whatever owns the endpoint (here: the same still-serving stub,
            // because the test daemon has no process of its own to exit).
            let result = manager.restart(repo.to_str().unwrap()).await;
            if atomic_supported {
                assert!(
                    stop_commands(&commands).is_empty(),
                    "the atomic path needs no fire-and-forget command: {:?}",
                    stop_commands(&commands)
                );
            } else {
                assert!(
                    result.is_err(),
                    "Unsupported must not restart an idle owner"
                );
                assert!(
                    stop_commands(&commands).is_empty(),
                    "no legacy shutdown command"
                );
            }
        }
    }

    #[tokio::test]
    async fn add_rejects_a_missing_directory() {
        let dir = tempfile::tempdir().unwrap();
        let (manager, _router) = manager_in(dir.path(), dir.path().join("no.sock"));
        let error = manager.add("/does/not/exist").await.unwrap_err();
        assert!(error.message.contains("不是目录"), "{error}");
    }

    /// Bind AND serve a stub daemon on `socket` — a bound-but-unserved socket
    /// would hang the client's first request forever. Unix-socket only.
    #[cfg(unix)]
    async fn serve_stub_daemon(socket: &Path) -> tokio_util::sync::CancellationToken {
        let server = LocalSocketServer::bind(socket, StubService::new())
            .await
            .expect("test daemon binds");
        let shutdown = tokio_util::sync::CancellationToken::new();
        let serve_shutdown = shutdown.clone();
        tokio::spawn(async move {
            let _ = server.serve(serve_shutdown).await;
        });
        shutdown
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn probe_attaches_to_a_live_daemon_and_registry_persists() {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("daemon.sock");
        // A live "daemon": a real Unix-socket server over a stub service.
        let _shutdown = serve_stub_daemon(&socket).await;
        let repo = dir.path().join("project-b");
        std::fs::create_dir_all(&repo).unwrap();

        let (manager, router) = manager_in(dir.path(), socket);
        let info = manager.add(repo.to_str().unwrap()).await.expect("attaches");
        assert_eq!(info.status, ProjectStatus::Online);
        assert_eq!(info.name, "project-b");
        let canonical_repo = repo.canonicalize().unwrap();
        assert!(
            router.handles(&canonical_repo),
            "router must route the repo"
        );

        // Registry landed on disk with the canonical path.
        let registry: Registry = serde_json::from_slice(
            &std::fs::read(dir.path().join("state/web/projects.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(registry.projects, vec![canonical_repo.clone()]);

        // list(): primary first, then the registered project.
        let list = manager.list();
        assert_eq!(list.len(), 2);
        assert_eq!(list[1].path, canonical_repo.display().to_string());

        // remove(): unrouted + registry emptied.
        manager
            .remove(repo.to_str().unwrap())
            .await
            .expect("removes cleanly");
        assert!(!router.handles(&canonical_repo));
        let registry: Registry = serde_json::from_slice(
            &std::fs::read(dir.path().join("state/web/projects.json")).unwrap(),
        )
        .unwrap();
        assert!(registry.projects.is_empty());
    }

    #[tokio::test]
    async fn add_without_daemon_or_spawner_reports_offline_and_stays_registered() {
        let dir = tempfile::tempdir().unwrap();
        let (manager, _router) = manager_in(dir.path(), dir.path().join("dead.sock"));
        let repo = dir.path().join("project-c");
        std::fs::create_dir_all(&repo).unwrap();

        let mut statuses = manager.subscribe_status();
        let error = manager.add(repo.to_str().unwrap()).await.unwrap_err();
        assert!(error.message.contains("无法代为启动"), "{error}");
        // Status walked starting → offline, and the entry survives for retry.
        assert_eq!(statuses.recv().await.unwrap().1, ProjectStatus::Starting);
        assert_eq!(statuses.recv().await.unwrap().1, ProjectStatus::Offline);
        let list = manager.list();
        assert_eq!(list.len(), 2);
        assert_eq!(list[1].status, ProjectStatus::Offline);
    }

    /// A runtime built from DIFFERENT sources, still serving the endpoint.
    ///
    /// This is the Web defect Gate 2 fixes: the probe-or-spawn path attached to
    /// it without asking what it was, so a browser served new work through a
    /// runtime this build does not match.
    struct ForeignBuildService {
        commands: Arc<Mutex<Vec<String>>>,
        retire: leveler_client_protocol::RetireDecision,
    }

    impl ForeignBuildService {
        fn new(retire: leveler_client_protocol::RetireDecision) -> Arc<Self> {
            Arc::new(Self {
                commands: Arc::new(Mutex::new(Vec::new())),
                retire,
            })
        }
    }

    #[async_trait]
    impl InteractiveRuntimeClient for ForeignBuildService {
        async fn send(&self, command: ClientCommand) -> Result<(), ClientError> {
            self.commands.lock().unwrap().push(format!("{command:?}"));
            Ok(())
        }

        fn subscribe(&self) -> broadcast::Receiver<RuntimeEvent> {
            broadcast::channel(4).1
        }

        async fn snapshot(&self, _session: &SessionId) -> Result<UiSessionSnapshot, ClientError> {
            Err(ClientError::Runtime("not exercised".to_string()))
        }
    }

    #[async_trait]
    impl LocalRuntimeService for ForeignBuildService {
        async fn create_session(
            &self,
            _request: CreateSessionRequest,
        ) -> Result<SessionBootstrap, ClientError> {
            Err(ClientError::Runtime("not exercised".to_string()))
        }

        async fn runtime_info(&self) -> Result<leveler_client_protocol::RuntimeInfo, ClientError> {
            let mut build = leveler_core::BuildIdentity::current();
            build.fingerprint.push_str("-other-generation");
            Ok(leveler_client_protocol::RuntimeInfo {
                runtime_id: leveler_core::RuntimeId::new("foreign-generation-runtime"),
                version: env!("CARGO_PKG_VERSION").to_string(),
                build,
                config_fingerprint: None,
                // This test process: a pid the migration must refuse to signal.
                pid: std::process::id(),
                health: leveler_client_protocol::RuntimeHealth {
                    accepting_work: true,
                    quiescent: false,
                    active_turns: 1,
                    ..Default::default()
                },
            })
        }

        async fn try_retire_if_idle(
            &self,
            _request: leveler_client_protocol::RetireRequest,
        ) -> Result<leveler_client_protocol::RetireDecision, ClientError> {
            Ok(self.retire)
        }
    }

    /// WEB-GENERATION-1 — the Web must NOT silently attach to a runtime of a
    /// different generation, and must NOT stop it behind the operator's back.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_busy_runtime_of_another_generation_is_deferred_not_adopted() {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("foreign.sock");
        let service = ForeignBuildService::new(leveler_client_protocol::RetireDecision::Busy {
            active_turns: 1,
            active_background_tasks: 0,
        });
        let commands = service.commands.clone();
        let server = LocalSocketServer::bind(&socket, service).await.unwrap();
        let shutdown = tokio_util::sync::CancellationToken::new();
        let serve_shutdown = shutdown.clone();
        tokio::spawn(async move {
            let _ = server.serve(serve_shutdown).await;
        });

        let repo = dir.path().join("foreign-project");
        std::fs::create_dir_all(&repo).unwrap();
        let (manager, router) = manager_in(dir.path(), socket.clone());
        let error = manager
            .add(repo.to_str().unwrap())
            .await
            .expect_err("a runtime of another generation must not be adopted");
        assert_eq!(
            error.state,
            Some(
                leveler_runtime_host::RuntimeLifecycleState::UpgradeDeferred {
                    active_turns: 1,
                    active_background_tasks: 0,
                }
            ),
            "the deferral must be named, not swallowed: {error}"
        );
        assert!(
            !router.handles(&repo.canonicalize().unwrap()),
            "new work must not be routed through a runtime the Web does not match"
        );
        assert!(
            commands.lock().unwrap().is_empty(),
            "a busy runtime must not be commanded or stopped: {:?}",
            commands.lock().unwrap()
        );
        let client = LocalSocketRuntimeClient::connect(&socket)
            .await
            .expect("the deferred runtime keeps serving");
        assert_eq!(
            LocalRuntimeService::runtime_info(&client)
                .await
                .unwrap()
                .pid,
            std::process::id(),
            "no replacement may take an endpoint whose owner was not released"
        );
        shutdown.cancel();
    }

    /// WEB-GENERATION-2 — a legacy runtime of another generation that still
    /// owns work is deferred, regardless of whether its PID is identifiable.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_busy_unsupported_runtime_of_another_generation_is_deferred() {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("legacy.sock");
        let service =
            ForeignBuildService::new(leveler_client_protocol::RetireDecision::Unsupported);
        let server = LocalSocketServer::bind(&socket, service).await.unwrap();
        let shutdown = tokio_util::sync::CancellationToken::new();
        let serve_shutdown = shutdown.clone();
        tokio::spawn(async move {
            let _ = server.serve(serve_shutdown).await;
        });

        let repo = dir.path().join("legacy-project");
        std::fs::create_dir_all(&repo).unwrap();
        let (manager, router) = manager_in(dir.path(), socket);
        let error = manager
            .add(repo.to_str().unwrap())
            .await
            .expect_err("an unprovable runtime must not be replaced");
        assert_eq!(
            error.state,
            Some(
                leveler_runtime_host::RuntimeLifecycleState::UpgradeDeferred {
                    active_turns: 1,
                    active_background_tasks: 0,
                }
            ),
            "the legacy busy deferral must be named: {error}"
        );
        assert!(!router.handles(&repo.canonicalize().unwrap()));
        // This test process is still alive: nothing was signalled.
        shutdown.cancel();
    }

    #[tokio::test]
    async fn rename_sets_and_clears_a_persisted_alias() {
        let dir = tempfile::tempdir().unwrap();
        let (manager, _router) = manager_in(dir.path(), dir.path().join("dead.sock"));
        let repo = dir.path().join("project-e");
        std::fs::create_dir_all(&repo).unwrap();
        let canonical_repo = repo.canonicalize().unwrap();

        // Unregistered paths are rejected.
        let error = manager.rename(repo.to_str().unwrap(), "x").unwrap_err();
        assert!(error.message.contains("未注册的项目"), "{error}");

        // Register (no daemon, no spawner: stays offline but listed).
        let _ = manager.add(repo.to_str().unwrap()).await;

        manager
            .rename(repo.to_str().unwrap(), "别名")
            .expect("renames");
        assert_eq!(manager.list()[1].name, "别名");

        // The alias is persisted alongside the paths.
        let registry: Registry = serde_json::from_slice(
            &std::fs::read(dir.path().join("state/web/projects.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(
            registry.aliases.get(&canonical_repo).map(String::as_str),
            Some("别名")
        );

        // An empty name clears the alias back to the path-derived short name.
        manager
            .rename(repo.to_str().unwrap(), "  ")
            .expect("clears");
        assert_eq!(manager.list()[1].name, "project-e");

        // The primary can be aliased too.
        manager
            .rename(dir.path().join("primary").to_str().unwrap(), "主项目")
            .expect("renames primary");
        assert_eq!(manager.list()[0].name, "主项目");
    }

    #[tokio::test]
    async fn load_registry_restores_aliases() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path().join("project-f");
        std::fs::create_dir_all(&repo).unwrap();
        let canonical_repo = repo.canonicalize().unwrap();
        std::fs::create_dir_all(dir.path().join("state/web")).unwrap();
        std::fs::write(
            dir.path().join("state/web/projects.json"),
            serde_json::to_vec(&Registry {
                projects: vec![canonical_repo.clone()],
                aliases: HashMap::from([(canonical_repo, "历史别名".to_string())]),
                ignored: Vec::new(),
            })
            .unwrap(),
        )
        .unwrap();

        let (manager, _router) = manager_in(dir.path(), dir.path().join("dead.sock"));
        manager.clone().load_registry().await;
        // The project itself failed to come online (dead socket, no spawner)
        // but the alias survived the round-trip.
        assert_eq!(manager.list()[1].name, "历史别名");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn load_registry_reattaches_persisted_projects() {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("daemon.sock");
        let _shutdown = serve_stub_daemon(&socket).await;
        let repo = dir.path().join("project-d");
        std::fs::create_dir_all(&repo).unwrap();
        let canonical_repo = repo.canonicalize().unwrap();
        std::fs::create_dir_all(dir.path().join("state/web")).unwrap();
        std::fs::write(
            dir.path().join("state/web/projects.json"),
            serde_json::to_vec(&Registry {
                projects: vec![canonical_repo.clone()],
                aliases: HashMap::new(),
                ignored: Vec::new(),
            })
            .unwrap(),
        )
        .unwrap();

        let (manager, router) = manager_in(dir.path(), socket);
        manager.clone().load_registry().await;
        assert!(router.handles(&canonical_repo));
        assert_eq!(manager.list()[1].status, ProjectStatus::Online);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn discover_historical_projects_registers_marked_repos() {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("daemon.sock");
        let _shutdown = serve_stub_daemon(&socket).await;
        // A historical repo: state dir under <home>/projects with an owner
        // marker, but absent from state/web/projects.json.
        let repo = dir.path().join("tui-only-project");
        std::fs::create_dir_all(&repo).unwrap();
        let canonical_repo = repo.canonicalize().unwrap();
        let state_dir = dir.path().join("state").join("projects").join("-some-slug");
        std::fs::create_dir_all(&state_dir).unwrap();
        std::fs::write(
            state_dir.join(".repository-root"),
            canonical_repo.display().to_string(),
        )
        .unwrap();

        let (manager, router) = manager_in(dir.path(), socket);
        manager
            .discover_historical_projects(&dir.path().join("tempzone"))
            .await;
        assert!(router.handles(&canonical_repo));
        let list = manager.list();
        assert_eq!(list.len(), 2);
        assert_eq!(list[1].path, canonical_repo.display().to_string());
        assert_eq!(list[1].status, ProjectStatus::Online);
    }

    /// State dirs created before the ownership marker existed carry no
    /// `.repository-root` — but their `sessions.db` records the repository.
    /// Discovery must list those too (they are exactly the TUI-only history
    /// the sidebar was missing), while never persisting discovered projects
    /// into the registry.
    #[cfg(unix)]
    #[tokio::test]
    async fn discovery_falls_back_to_sessions_db_and_stays_ephemeral() {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("daemon.sock");
        let _shutdown = serve_stub_daemon(&socket).await;
        let repo = dir.path().join("legacy-project");
        std::fs::create_dir_all(&repo).unwrap();
        let canonical_repo = repo.canonicalize().unwrap();
        // Legacy state dir: sessions.db only, no marker.
        let state_dir = dir
            .path()
            .join("state")
            .join("projects")
            .join("-legacy-slug");
        std::fs::create_dir_all(&state_dir).unwrap();
        {
            let db = leveler_storage::Database::connect(&state_dir.join("sessions.db"))
                .await
                .unwrap();
            leveler_storage::SessionRepository::new(&db)
                .create(&leveler_storage::SessionRecord::new(
                    canonical_repo.display().to_string(),
                    "goal",
                    "model",
                    leveler_core::now(),
                ))
                .await
                .unwrap();
        }

        let (manager, router) = manager_in(dir.path(), socket);
        manager
            .discover_historical_projects(&dir.path().join("tempzone"))
            .await;
        assert!(router.handles(&canonical_repo), "db-recorded repo attaches");
        let list = manager.list();
        assert_eq!(list.len(), 2, "{list:?}");
        assert_eq!(list[1].path, canonical_repo.display().to_string());
        assert_eq!(list[1].status, ProjectStatus::Online);
        // Discovery is ephemeral: no registry write.
        assert!(
            !dir.path().join("state/web/projects.json").exists(),
            "discovery must not persist the registry"
        );
    }

    /// Deleted checkouts and temp eval dirs leave state behind; discovery must
    /// skip repositories that no longer exist instead of listing dead rows.
    #[tokio::test]
    async fn discovery_skips_vanished_repositories() {
        let dir = tempfile::tempdir().unwrap();
        let state_dir = dir.path().join("state").join("projects").join("-gone-slug");
        std::fs::create_dir_all(&state_dir).unwrap();
        std::fs::write(
            state_dir.join(".repository-root"),
            dir.path().join("deleted-checkout").display().to_string(),
        )
        .unwrap();

        let (manager, _router) = manager_in(dir.path(), dir.path().join("dead.sock"));
        manager
            .discover_historical_projects(&dir.path().join("tempzone"))
            .await;
        assert_eq!(manager.list().len(), 1, "vanished repo must not be listed");
    }

    /// Repos under the ephemeral root (the OS temp dir in production) are
    /// throwaway eval/fixture checkouts — they must not bury the real
    /// projects. Explicit `add` still accepts such a path.
    #[tokio::test]
    async fn discovery_skips_repositories_under_the_ephemeral_root() {
        let dir = tempfile::tempdir().unwrap();
        let ephemeral = dir.path().join("tempzone");
        let repo = ephemeral.join("leveler-eval-rust-h1-12345");
        std::fs::create_dir_all(&repo).unwrap();
        let state_dir = dir.path().join("state").join("projects").join("-eval-slug");
        std::fs::create_dir_all(&state_dir).unwrap();
        std::fs::write(
            state_dir.join(".repository-root"),
            repo.canonicalize().unwrap().display().to_string(),
        )
        .unwrap();

        let (manager, _router) = manager_in(dir.path(), dir.path().join("dead.sock"));
        manager.discover_historical_projects(&ephemeral).await;
        assert_eq!(
            manager.list().len(),
            1,
            "temp checkout must not be discovered"
        );
        // …but the user can still open it deliberately.
        let _ = manager.add(repo.to_str().unwrap()).await;
        assert_eq!(manager.list().len(), 2);
    }

    /// 移除 must stick: a removed project is recorded in the registry's
    /// ignore list, so discovery (this run AND the next server start) stops
    /// re-listing it — its state dir under `<home>/projects` still exists.
    /// Explicitly opening the path again clears the ignore.
    #[tokio::test]
    async fn removed_project_is_not_rediscovered() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path().join("removed-project");
        std::fs::create_dir_all(&repo).unwrap();
        let canonical_repo = repo.canonicalize().unwrap();
        let state_dir = dir
            .path()
            .join("state")
            .join("projects")
            .join("-removed-slug");
        std::fs::create_dir_all(&state_dir).unwrap();
        std::fs::write(
            state_dir.join(".repository-root"),
            canonical_repo.display().to_string(),
        )
        .unwrap();

        let tempzone = dir.path().join("tempzone");
        let (manager, _router) = manager_in(dir.path(), dir.path().join("dead.sock"));
        manager.discover_historical_projects(&tempzone).await;
        assert_eq!(manager.list().len(), 2, "discovered before removal");

        manager
            .remove(canonical_repo.to_str().unwrap())
            .await
            .expect("removes");
        // Same run: a re-discovery pass must not resurrect it.
        manager.discover_historical_projects(&tempzone).await;
        assert_eq!(manager.list().len(), 1, "removed project must stay gone");

        // Fresh server start: load_registry + discovery still honors the
        // persisted ignore.
        let (manager2, _router2) = manager_in(dir.path(), dir.path().join("dead.sock"));
        manager2.clone().load_registry().await;
        manager2.discover_historical_projects(&tempzone).await;
        assert_eq!(
            manager2.list().len(),
            1,
            "ignore must survive a restart: {:?}",
            manager2.list()
        );

        // Explicit open clears the ignore (user changed their mind).
        let _ = manager2.add(canonical_repo.to_str().unwrap()).await;
        assert_eq!(manager2.list().len(), 2, "explicit open re-registers");
    }

    /// A discovered (offline, ephemeral) project becomes a real registry
    /// member the moment the user opens it explicitly — even when bringing it
    /// online fails, so the restart button has something to retry.
    #[tokio::test]
    async fn opening_a_discovered_project_promotes_it_to_the_registry() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path().join("promoted-project");
        std::fs::create_dir_all(&repo).unwrap();
        let canonical_repo = repo.canonicalize().unwrap();
        let state_dir = dir
            .path()
            .join("state")
            .join("projects")
            .join("-promoted-slug");
        std::fs::create_dir_all(&state_dir).unwrap();
        std::fs::write(
            state_dir.join(".repository-root"),
            canonical_repo.display().to_string(),
        )
        .unwrap();

        let (manager, _router) = manager_in(dir.path(), dir.path().join("dead.sock"));
        manager
            .discover_historical_projects(&dir.path().join("tempzone"))
            .await;
        assert_eq!(manager.list()[1].status, ProjectStatus::Offline);
        assert!(!dir.path().join("state/web/projects.json").exists());

        // Explicit open: no daemon and no spawner, so it errors — but the
        // project is now persisted for the retry path.
        let _ = manager.add(canonical_repo.to_str().unwrap()).await;
        let registry: Registry = serde_json::from_slice(
            &std::fs::read(dir.path().join("state/web/projects.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(registry.projects, vec![canonical_repo]);
    }
}
