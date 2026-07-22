//! Project registry + daemon lifecycle for the multi-project WebUI.
//!
//! The aggregation server keeps a registry of repositories the user opened
//! (`<leveler home>/web-projects.json`) and brings each one online behind the
//! [`RouterService`]: probe the repo's per-daemon Unix socket first and attach
//! to a live daemon when one exists (e.g. the user's own `leveler tui`),
//! otherwise spawn `leveler --repo <path> serve --ready-json <file>` and
//! connect once the readiness file appears. Spawned children are killed when
//! their handle drops (web exit); after a hard kill of the web process the
//! orphaned daemon still owns its Unix socket, so the next web start reattaches
//! to it instead of double-starting.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use tokio::sync::{broadcast, oneshot};

use leveler_local_transport::LocalSocketRuntimeClient;
use leveler_project::Layout;

use crate::router::RouterService;

/// Where a registered project's daemon currently stands. Serialized lowercase
/// on both the REST payloads and the WS `project_status` frames.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ProjectStatus {
    Online,
    Starting,
    Offline,
}

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
#[derive(Debug, thiserror::Error)]
#[error("{0}")]
pub struct ProjectError(pub String);

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
}

/// Runtime state of one registered project.
struct Entry {
    status: ProjectStatus,
    /// Signals the monitor task (which owns the spawned [`tokio::process::Child`])
    /// to kill it. `None` when the daemon was attached, not spawned — someone
    /// else's process is never killed from here.
    kill: Option<oneshot::Sender<()>>,
}

/// Shared with monitor tasks so they never own the manager itself.
struct State {
    entries: Mutex<HashMap<PathBuf, Entry>>,
    /// Display aliases (loaded from / persisted with the registry).
    aliases: Mutex<HashMap<PathBuf, String>>,
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
        registry_path: PathBuf,
        exe: Option<PathBuf>,
    ) -> Arc<Self> {
        Self::with_socket_resolver(router, registry_path, exe, |repo| {
            Layout::resolve(repo.to_path_buf(), None).socket_path()
        })
    }

    pub fn with_socket_resolver(
        router: Arc<RouterService>,
        registry_path: PathBuf,
        exe: Option<PathBuf>,
        socket_for: impl Fn(&Path) -> PathBuf + Send + Sync + 'static,
    ) -> Arc<Self> {
        Arc::new(Self {
            router,
            registry_path,
            exe,
            socket_for: Box::new(socket_for),
            state: Arc::new(State {
                entries: Mutex::new(HashMap::new()),
                aliases: Mutex::new(HashMap::new()),
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
    /// answers with its current state.
    pub async fn add(&self, path: &str) -> Result<ProjectInfo, ProjectError> {
        let repo = PathBuf::from(path);
        if !repo.is_dir() {
            return Err(ProjectError(format!("不是目录：{path}")));
        }
        let repo = repo.canonicalize().unwrap_or(repo);
        if repo == self.router.primary_repo() {
            return Ok(self.info_for(&repo, ProjectStatus::Online));
        }
        {
            let entries = self.state.entries.lock().unwrap();
            if let Some(entry) = entries.get(&repo) {
                let status = entry.status;
                drop(entries);
                return Ok(self.info_for(&repo, status));
            }
        }
        self.state.entries.lock().unwrap().insert(
            repo.clone(),
            Entry {
                status: ProjectStatus::Starting,
                kill: None,
            },
        );
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

    /// Unregister a project: kill a spawned daemon, drop its sessions from the
    /// merged list, persist. Attached daemons (not spawned here) are left
    /// running — they belong to someone else.
    pub fn remove(&self, path: &str) -> Result<(), ProjectError> {
        let repo = canonical(path);
        let entry = self.state.entries.lock().unwrap().remove(&repo);
        let Some(entry) = entry else {
            return Err(ProjectError(format!("未注册的项目：{path}")));
        };
        if let Some(kill) = entry.kill {
            let _ = kill.send(());
        }
        self.state.aliases.lock().unwrap().remove(&repo);
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
            return Err(ProjectError(format!("未注册的项目：{path}")));
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

    /// Restart a project's daemon: kill our spawned child if any, then go
    /// through the probe-or-spawn path again.
    pub async fn restart(&self, path: &str) -> Result<(), ProjectError> {
        let repo = canonical(path);
        {
            let mut entries = self.state.entries.lock().unwrap();
            let Some(entry) = entries.get_mut(&repo) else {
                return Err(ProjectError(format!("未注册的项目：{path}")));
            };
            if let Some(kill) = entry.kill.take() {
                let _ = kill.send(());
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
    /// (derived from the registry path's parent) and register each one. Runs
    /// after [`load_registry`](Self::load_registry) at server start; `add` is
    /// idempotent, so already-registered projects are untouched. This is what
    /// lets the sidebar list projects the user only ever drove from the TUI.
    pub async fn discover_historical_projects(&self) {
        let Some(home) = self.registry_path.parent() else {
            return;
        };
        for repo in leveler_project::layout::known_repositories(home) {
            let Some(repo_str) = repo.to_str() else {
                continue;
            };
            if let Err(error) = self.add(repo_str).await {
                tracing::warn!(%error, repo = %repo.display(), "failed to bring a discovered project online");
            }
        }
    }

    /// Probe the repo's daemon socket; attach when live, else spawn a fresh
    /// daemon and connect once it reports ready.
    async fn bring_online(&self, repo: &Path) -> Result<(), ProjectError> {
        let socket = (self.socket_for)(repo);
        if let Ok(client) = LocalSocketRuntimeClient::connect(&socket).await {
            self.router
                .add_daemon(repo.to_path_buf(), Arc::new(client))
                .await;
            return Ok(());
        }
        let Some(exe) = &self.exe else {
            return Err(ProjectError(format!(
                "项目没有运行中的 daemon（{}），且当前环境无法代为启动",
                socket.display()
            )));
        };
        let ready_path = std::env::temp_dir().join(format!(
            "leveler-web-ready-{}-{}.json",
            std::process::id(),
            path_nonce(repo)
        ));
        let _ = std::fs::remove_file(&ready_path);
        let mut child = tokio::process::Command::new(exe)
            .arg("--repo")
            .arg(repo)
            .arg("serve")
            .arg("--ready-json")
            .arg(&ready_path)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .kill_on_drop(true)
            .spawn()
            .map_err(|error| ProjectError(format!("启动 daemon 失败：{error}")))?;

        let socket = match wait_ready(&ready_path, &mut child).await {
            Ok(socket) => socket,
            Err(error) => {
                let _ = child.start_kill();
                return Err(error);
            }
        };
        let _ = std::fs::remove_file(&ready_path);

        let client = connect_with_retry(&socket).await.map_err(|error| {
            let _ = child.start_kill();
            ProjectError(format!("daemon 已就绪但连接失败：{error}"))
        })?;
        self.router
            .add_daemon(repo.to_path_buf(), Arc::new(client))
            .await;

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
            projects: {
                let mut paths: Vec<PathBuf> =
                    self.state.entries.lock().unwrap().keys().cloned().collect();
                paths.sort();
                paths
            },
            aliases: self.state.aliases.lock().unwrap().clone(),
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

/// Wait for the daemon's readiness file and return the socket path it reports.
/// A child that exits first fails fast with its status instead of burning the
/// whole timeout.
async fn wait_ready(
    ready_path: &Path,
    child: &mut tokio::process::Child,
) -> Result<PathBuf, ProjectError> {
    const READY_TIMEOUT: Duration = Duration::from_secs(20);
    const POLL: Duration = Duration::from_millis(100);
    let mut waited = Duration::ZERO;
    loop {
        if let Ok(bytes) = std::fs::read(ready_path) {
            let ready: serde_json::Value = serde_json::from_slice(&bytes)
                .map_err(|error| ProjectError(format!("无法解析 daemon 就绪信息：{error}")))?;
            let socket = ready
                .get("socket")
                .and_then(|v| v.as_str())
                .ok_or_else(|| ProjectError("daemon 就绪信息缺少 socket 字段".to_string()))?;
            return Ok(PathBuf::from(socket));
        }
        if let Ok(Some(status)) = child.try_wait() {
            return Err(ProjectError(format!(
                "daemon 启动即退出（{status}）——多半是该仓库已有 daemon 或配置错误"
            )));
        }
        if waited >= READY_TIMEOUT {
            return Err(ProjectError("等待 daemon 就绪超时".to_string()));
        }
        tokio::time::sleep(POLL).await;
        waited += POLL;
    }
}

/// The readiness file is written after the socket is bound, so the first
/// attempt normally succeeds; the retries cover scheduler noise only.
async fn connect_with_retry(
    socket: &Path,
) -> Result<LocalSocketRuntimeClient, leveler_local_transport::TransportError> {
    let mut last = None;
    for _ in 0..5 {
        match LocalSocketRuntimeClient::connect(socket).await {
            Ok(client) => return Ok(client),
            Err(error) => last = Some(error),
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    Err(last.expect("at least one attempt ran"))
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
        _ = kill => {
            let _ = child.start_kill();
            let _ = child.wait().await;
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
    use leveler_local_transport::LocalSocketServer;

    /// Minimal primary service: the manager tests never exercise commands.
    struct StubService {
        mock: MockRuntimeClient,
    }

    impl StubService {
        fn new() -> Arc<Self> {
            Arc::new(Self {
                mock: MockRuntimeClient::new(SessionId::new("stub")),
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
    }

    fn manager_in(dir: &Path, socket: PathBuf) -> (Arc<ProjectManager>, Arc<RouterService>) {
        let router = RouterService::new(StubService::new(), dir.join("primary"));
        let manager = ProjectManager::with_socket_resolver(
            router.clone(),
            dir.join("web-projects.json"),
            None,
            move |_repo| socket.clone(),
        );
        (manager, router)
    }

    #[tokio::test]
    async fn add_rejects_a_missing_directory() {
        let dir = tempfile::tempdir().unwrap();
        let (manager, _router) = manager_in(dir.path(), dir.path().join("no.sock"));
        let error = manager.add("/does/not/exist").await.unwrap_err();
        assert!(error.0.contains("不是目录"), "{error}");
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
        let registry: Registry =
            serde_json::from_slice(&std::fs::read(dir.path().join("web-projects.json")).unwrap())
                .unwrap();
        assert_eq!(registry.projects, vec![canonical_repo.clone()]);

        // list(): primary first, then the registered project.
        let list = manager.list();
        assert_eq!(list.len(), 2);
        assert_eq!(list[1].path, canonical_repo.display().to_string());

        // remove(): unrouted + registry emptied.
        manager
            .remove(repo.to_str().unwrap())
            .expect("removes cleanly");
        assert!(!router.handles(&canonical_repo));
        let registry: Registry =
            serde_json::from_slice(&std::fs::read(dir.path().join("web-projects.json")).unwrap())
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
        assert!(error.0.contains("无法代为启动"), "{error}");
        // Status walked starting → offline, and the entry survives for retry.
        assert_eq!(statuses.recv().await.unwrap().1, ProjectStatus::Starting);
        assert_eq!(statuses.recv().await.unwrap().1, ProjectStatus::Offline);
        let list = manager.list();
        assert_eq!(list.len(), 2);
        assert_eq!(list[1].status, ProjectStatus::Offline);
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
        assert!(error.0.contains("未注册的项目"), "{error}");

        // Register (no daemon, no spawner: stays offline but listed).
        let _ = manager.add(repo.to_str().unwrap()).await;

        manager
            .rename(repo.to_str().unwrap(), "别名")
            .expect("renames");
        assert_eq!(manager.list()[1].name, "别名");

        // The alias is persisted alongside the paths.
        let registry: Registry =
            serde_json::from_slice(&std::fs::read(dir.path().join("web-projects.json")).unwrap())
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
        std::fs::write(
            dir.path().join("web-projects.json"),
            serde_json::to_vec(&Registry {
                projects: vec![canonical_repo.clone()],
                aliases: HashMap::from([(canonical_repo, "历史别名".to_string())]),
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
        std::fs::write(
            dir.path().join("web-projects.json"),
            serde_json::to_vec(&Registry {
                projects: vec![canonical_repo.clone()],
                aliases: HashMap::new(),
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
        // marker, but absent from web-projects.json.
        let repo = dir.path().join("tui-only-project");
        std::fs::create_dir_all(&repo).unwrap();
        let canonical_repo = repo.canonicalize().unwrap();
        let state_dir = dir.path().join("projects").join("-some-slug");
        std::fs::create_dir_all(&state_dir).unwrap();
        std::fs::write(
            state_dir.join(".repository-root"),
            canonical_repo.display().to_string(),
        )
        .unwrap();

        let (manager, router) = manager_in(dir.path(), socket);
        manager.discover_historical_projects().await;
        assert!(router.handles(&canonical_repo));
        let list = manager.list();
        assert_eq!(list.len(), 2);
        assert_eq!(list[1].path, canonical_repo.display().to_string());
        assert_eq!(list[1].status, ProjectStatus::Online);
    }
}
