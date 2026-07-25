//! Which project a remote frame belongs to, and where that project's runtime is.
//!
//! One pairing covers a machine, not a repository. The phone lists the projects
//! the user has open and switches between them, exactly as the browser UI does —
//! so the agent needs the browser UI's shape without the browser UI's crate:
//! read the same registry, resolve the same per-repository socket, and attach to
//! whatever daemon is already serving it.
//!
//! Two things it deliberately does not do:
//!
//! - **Spawn daemons.** The web server owns that lifecycle. An agent that also
//!   spawned would race it for the socket and leave orphans behind when the
//!   phone disconnected. Attach or report offline.
//! - **Accept a path from the network.** Only repositories already in the local
//!   registry are reachable; "open this path" from a phone is a later phase with
//!   its own confirmation UX, because it is a request to run code somewhere new.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;
use leveler_local_transport::LocalRuntimeService;
use leveler_session_wire::ProjectStatus;
use serde::{Deserialize, Serialize};

/// One project as the phone sees it.
///
/// `path_display` is a basename by default rather than the full path: the phone
/// is a screen someone else can read, and the absolute layout of a developer's
/// disk is not something a remote surface needs to publish.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProjectInfo {
    /// Stable across restarts: the same hash the repository's socket is named
    /// after, so an id resolves to one repository and nothing else.
    pub project_id: String,
    pub path_display: String,
    pub status: ProjectStatus,
}

/// Why a frame could not be routed to a runtime.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum RouteError {
    #[error("this host has no open project with that id")]
    UnknownProject,
    #[error("that project has no runtime running on the host")]
    ProjectOffline,
    #[error("more than one project is open, so the target must be named")]
    ProjectRequired,
}

impl RouteError {
    pub fn code(&self) -> &'static str {
        match self {
            RouteError::UnknownProject => "unknown_project",
            RouteError::ProjectOffline => "project_offline",
            RouteError::ProjectRequired => "project_required",
        }
    }
}

/// Where the agent finds the runtime serving a project.
#[async_trait]
pub trait ProjectRoutes: Send + Sync {
    /// The projects this host currently exposes, online or not. An offline
    /// project stays listed: the phone shows it greyed out rather than having it
    /// silently vanish.
    async fn projects(&self) -> Vec<ProjectInfo>;

    /// The runtime for `project_id`.
    async fn runtime(&self, project_id: &str) -> Result<Arc<dyn LocalRuntimeService>, RouteError>;

    /// The project a caller means when it names none.
    ///
    /// `Some` only when exactly one project is open. With several, guessing
    /// would deliver a command to the wrong repository, so the caller is told to
    /// be explicit instead.
    async fn implied_project(&self) -> Result<String, RouteError>;
}

/// One project, already attached. The shape a single-repository host has, and
/// what tests use when the question under test is not routing.
pub struct SingleProject {
    project_id: String,
    path_display: String,
    runtime: Arc<dyn LocalRuntimeService>,
}

impl SingleProject {
    pub fn new(
        project_id: impl Into<String>,
        path_display: impl Into<String>,
        runtime: Arc<dyn LocalRuntimeService>,
    ) -> Self {
        Self {
            project_id: project_id.into(),
            path_display: path_display.into(),
            runtime,
        }
    }
}

#[async_trait]
impl ProjectRoutes for SingleProject {
    async fn projects(&self) -> Vec<ProjectInfo> {
        vec![ProjectInfo {
            project_id: self.project_id.clone(),
            path_display: self.path_display.clone(),
            status: ProjectStatus::Online,
        }]
    }

    async fn runtime(&self, project_id: &str) -> Result<Arc<dyn LocalRuntimeService>, RouteError> {
        if project_id == self.project_id {
            Ok(self.runtime.clone())
        } else {
            Err(RouteError::UnknownProject)
        }
    }

    async fn implied_project(&self) -> Result<String, RouteError> {
        Ok(self.project_id.clone())
    }
}

/// The registry the browser UI writes when a user opens a project.
///
/// Read-only here, and every field defaults, so a registry written by a newer
/// web build stays readable rather than making every project disappear. The
/// authority for what is open remains that file; the agent adds nothing to it.
#[derive(Debug, Default, Deserialize)]
struct Registry {
    #[serde(default)]
    projects: Vec<PathBuf>,
    /// User-set display names, keyed by path.
    #[serde(default)]
    aliases: HashMap<PathBuf, String>,
    /// Repositories the user removed; they must not come back through this door.
    #[serde(default)]
    ignored: Vec<PathBuf>,
}

/// Display name for a repository: the user's alias, else the last path segment.
fn display_for(path: &Path, aliases: &HashMap<PathBuf, String>) -> String {
    aliases
        .get(path)
        .cloned()
        .or_else(|| {
            path.file_name()
                .map(|name| name.to_string_lossy().to_string())
        })
        .unwrap_or_else(|| path.display().to_string())
}

#[cfg(unix)]
pub use unix::ProjectRouter;

#[cfg(unix)]
mod unix {
    use std::sync::Mutex;

    use leveler_local_transport::{ClientKind, LocalSocketRuntimeClient};

    use super::*;

    /// The multi-project router: registry in, attached daemons out.
    pub struct ProjectRouter {
        registry_path: PathBuf,
        /// Repository path → its daemon socket. Injected rather than derived
        /// from the environment so a test can point at a temporary socket
        /// without mutating process-wide state.
        socket_for: Box<dyn Fn(&Path) -> PathBuf + Send + Sync>,
        /// Daemons already attached, keyed by project id. Attaching opens a
        /// subscription, so it is done once and shared by every stream for that
        /// project.
        attached: Mutex<HashMap<String, Arc<dyn LocalRuntimeService>>>,
    }

    impl ProjectRouter {
        pub fn new(
            registry_path: PathBuf,
            socket_for: impl Fn(&Path) -> PathBuf + Send + Sync + 'static,
        ) -> Self {
            Self {
                registry_path,
                socket_for: Box::new(socket_for),
                attached: Mutex::new(HashMap::new()),
            }
        }

        /// A project's id is its socket's file stem — the same repo-path hash the
        /// daemon is already keyed by. Deriving it rather than inventing one
        /// means the id cannot drift from the endpoint it names.
        fn project_id_for(&self, repo: &Path) -> Option<String> {
            (self.socket_for)(repo)
                .file_stem()
                .map(|stem| stem.to_string_lossy().to_string())
        }

        /// Open projects, freshest state each call: the registry is small and the
        /// user may have opened one a second ago in the browser UI.
        fn entries(&self) -> Vec<(String, PathBuf, String)> {
            let Ok(bytes) = std::fs::read(&self.registry_path) else {
                return Vec::new();
            };
            let registry: Registry = serde_json::from_slice(&bytes).unwrap_or_default();
            let ignored: std::collections::HashSet<&PathBuf> = registry.ignored.iter().collect();
            registry
                .projects
                .iter()
                .filter(|path| !ignored.contains(path))
                .filter_map(|path| {
                    let id = self.project_id_for(path)?;
                    Some((id, path.clone(), display_for(path, &registry.aliases)))
                })
                .collect()
        }

        /// Attach to a repository's daemon, or say it is offline.
        ///
        /// Declares [`ClientKind::Remote`], which is what keeps a phone from
        /// being counted as somebody sitting at the machine when the approval
        /// timeout asks who is watching.
        async fn attach(
            &self,
            repo: &Path,
            project_id: &str,
        ) -> Result<Arc<dyn LocalRuntimeService>, RouteError> {
            if let Some(runtime) = self.attached.lock().unwrap().get(project_id).cloned() {
                return Ok(runtime);
            }
            let socket = (self.socket_for)(repo);
            let client = LocalSocketRuntimeClient::connect_as(&socket, ClientKind::Remote)
                .await
                .map_err(|_| RouteError::ProjectOffline)?;
            let runtime: Arc<dyn LocalRuntimeService> = Arc::new(client);
            self.attached
                .lock()
                .unwrap()
                .insert(project_id.to_string(), runtime.clone());
            Ok(runtime)
        }
    }

    #[async_trait]
    impl ProjectRoutes for ProjectRouter {
        async fn projects(&self) -> Vec<ProjectInfo> {
            let mut listed = Vec::new();
            for (project_id, repo, path_display) in self.entries() {
                // Status is what attaching actually finds, not what a registry
                // file claims: a daemon that died leaves its entry behind.
                let status = match self.attach(&repo, &project_id).await {
                    Ok(_) => ProjectStatus::Online,
                    Err(_) => ProjectStatus::Offline,
                };
                listed.push(ProjectInfo {
                    project_id,
                    path_display,
                    status,
                });
            }
            listed
        }

        async fn runtime(
            &self,
            project_id: &str,
        ) -> Result<Arc<dyn LocalRuntimeService>, RouteError> {
            let (id, repo, _) = self
                .entries()
                .into_iter()
                .find(|(id, _, _)| id == project_id)
                .ok_or(RouteError::UnknownProject)?;
            self.attach(&repo, &id).await
        }

        async fn implied_project(&self) -> Result<String, RouteError> {
            let mut entries = self.entries().into_iter();
            match (entries.next(), entries.next()) {
                (Some((id, _, _)), None) => Ok(id),
                (Some(_), Some(_)) => Err(RouteError::ProjectRequired),
                (None, _) => Err(RouteError::UnknownProject),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The registry the web server writes, verbatim. If its shape changes, this
    /// is where the agent finds out — it reads the same file, and a silent
    /// mismatch would make every project vanish from the phone.
    const WEB_REGISTRY: &str = r#"{
      "projects": ["/repos/alpha", "/repos/beta", "/repos/gone"],
      "aliases": {"/repos/beta": "后端"},
      "ignored": ["/repos/gone"]
    }"#;

    #[test]
    fn the_web_registry_parses_with_aliases_and_removals() {
        let registry: Registry = serde_json::from_str(WEB_REGISTRY).unwrap();
        assert_eq!(registry.projects.len(), 3);
        assert_eq!(
            display_for(Path::new("/repos/beta"), &registry.aliases),
            "后端"
        );
        assert_eq!(
            display_for(Path::new("/repos/alpha"), &registry.aliases),
            "alpha",
            "no alias falls back to the basename, not the full path"
        );
        assert_eq!(registry.ignored, vec![PathBuf::from("/repos/gone")]);
    }

    /// An older or newer registry must not empty the project list.
    #[test]
    fn a_registry_with_only_paths_still_reads() {
        let registry: Registry = serde_json::from_str(r#"{"projects": ["/repos/alpha"]}"#).unwrap();
        assert_eq!(registry.projects, vec![PathBuf::from("/repos/alpha")]);
        assert!(registry.aliases.is_empty());
    }
}
