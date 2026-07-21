//! Multi-project aggregation: one [`LocalRuntimeService`] over many daemons.
//!
//! The WebUI server owns one in-process runtime for the repository it was
//! started in (the *primary*), plus zero or more socket-connected daemons for
//! other registered projects. [`RouterService`] presents all of them as a
//! single service to the REST/WS layers:
//!
//! - Session-targeted commands and snapshots route to the daemon that owns
//!   the session. Ownership is learned from merged session lists and snapshot
//!   probes and cached in a session → project map.
//! - `RequestSessionList` fans out to every daemon; each daemon's `SessionList`
//!   answer is cached per project and rebroadcast as one merged list.
//! - The global event stream ([`InteractiveRuntimeClient::subscribe`]) carries
//!   only cross-project facts — the merged `SessionList` and `RuntimeReady`.
//!   Session-internal events must be consumed per session via
//!   `subscribe_session`, so two browser tabs on different sessions (or
//!   projects) never see each other's traffic.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, RwLock};

use async_trait::async_trait;
use tokio::sync::broadcast;
use tokio::task::JoinHandle;

use leveler_client_protocol::{
    ClientCommand, ClientError, CommandEnvelope, InteractiveRuntimeClient, RuntimeEvent, SessionId,
    UiSessionSnapshot, UiSessionSummary,
};
use leveler_local_transport::{CreateSessionRequest, LocalRuntimeService, SessionBootstrap};

/// State shared with the per-daemon merger tasks, factored out so a task
/// never owns the router itself (no reference cycle).
struct Shared {
    /// Latest session list per project, keyed by canonical repository path.
    lists: RwLock<HashMap<PathBuf, Vec<UiSessionSummary>>>,
    /// Session ownership: session id → owning repository (primary or daemon).
    sessions: RwLock<HashMap<SessionId, PathBuf>>,
    /// The merged global stream handed to `subscribe()` callers.
    merged: broadcast::Sender<RuntimeEvent>,
}

impl Shared {
    /// Record `sessions` for `repo`, learn ownership, and rebroadcast the
    /// merged list (newest first). The router knows the owner authoritatively,
    /// so each summary's `repository` is stamped here — the frontend groups
    /// the merged list by it.
    fn apply_session_list(&self, repo: &Path, mut sessions: Vec<UiSessionSummary>) {
        for summary in &mut sessions {
            summary.repository = Some(repo.display().to_string());
        }
        {
            let mut ownership = self.sessions.write().unwrap();
            for summary in &sessions {
                ownership.insert(summary.id.clone(), repo.to_path_buf());
            }
        }
        self.lists
            .write()
            .unwrap()
            .insert(repo.to_path_buf(), sessions);
        self.broadcast_merged_list();
    }

    /// Drop everything cached for `repo` (daemon removed) and rebroadcast.
    fn forget_project(&self, repo: &Path) {
        self.lists.write().unwrap().remove(repo);
        self.sessions
            .write()
            .unwrap()
            .retain(|_, owner| owner != repo);
        self.broadcast_merged_list();
    }

    /// Concatenate all per-project lists into one, newest first, and emit it.
    fn broadcast_merged_list(&self) {
        let mut all: Vec<UiSessionSummary> = self
            .lists
            .read()
            .unwrap()
            .values()
            .flatten()
            .cloned()
            .collect();
        all.sort_by(|a, b| {
            b.updated_at
                .cmp(&a.updated_at)
                .then_with(|| a.id.as_str().cmp(b.id.as_str()))
        });
        let _ = self
            .merged
            .send(RuntimeEvent::SessionList { sessions: all });
    }
}

/// One [`LocalRuntimeService`] facade over the primary runtime plus every
/// registered project daemon.
pub struct RouterService {
    primary: Arc<dyn LocalRuntimeService>,
    /// Canonical repository the WebUI process itself serves.
    primary_repo: PathBuf,
    /// Other project daemons, keyed by canonical repository path.
    daemons: RwLock<HashMap<PathBuf, Arc<dyn LocalRuntimeService>>>,
    shared: Arc<Shared>,
    /// Per-daemon event merger tasks; they end when their daemon's stream
    /// closes (daemon removed / disconnected).
    mergers: Mutex<Vec<JoinHandle<()>>>,
}

impl RouterService {
    /// Wrap the in-process primary runtime. `primary_repo` is canonicalized
    /// best-effort so path comparisons match daemon registry keys.
    pub fn new(primary: Arc<dyn LocalRuntimeService>, primary_repo: PathBuf) -> Arc<Self> {
        let primary_repo = primary_repo.canonicalize().unwrap_or(primary_repo);
        let shared = Arc::new(Shared {
            lists: RwLock::new(HashMap::new()),
            sessions: RwLock::new(HashMap::new()),
            merged: broadcast::channel(256).0,
        });
        let router = Arc::new(Self {
            primary,
            primary_repo: primary_repo.clone(),
            daemons: RwLock::new(HashMap::new()),
            shared,
            mergers: Mutex::new(Vec::new()),
        });
        router.spawn_merger(primary_repo, router.primary.clone());
        router
    }

    /// The canonical path of the primary repository.
    pub fn primary_repo(&self) -> &Path {
        &self.primary_repo
    }

    /// Canonical repositories of all registered daemons (primary excluded),
    /// sorted for deterministic output.
    pub fn daemon_repos(&self) -> Vec<PathBuf> {
        let mut repos: Vec<PathBuf> = self.daemons.read().unwrap().keys().cloned().collect();
        repos.sort();
        repos
    }

    /// Sessions currently listed for `repo` (from the merged-list cache).
    pub fn session_count_for(&self, repo: &Path) -> usize {
        self.shared
            .lists
            .read()
            .unwrap()
            .get(repo)
            .map_or(0, Vec::len)
    }

    /// Whether `repo` is the primary or a registered daemon.
    pub fn handles(&self, repo: &Path) -> bool {
        repo == self.primary_repo || self.daemons.read().unwrap().contains_key(repo)
    }

    /// Register a project daemon and start merging its global event stream.
    /// A `RequestSessionList` is sent right away so the merged list (and the
    /// session-ownership map) reflects the new daemon without waiting for the
    /// frontend to ask.
    pub async fn add_daemon(&self, repo: PathBuf, service: Arc<dyn LocalRuntimeService>) {
        let repo = repo.canonicalize().unwrap_or(repo);
        self.daemons
            .write()
            .unwrap()
            .insert(repo.clone(), service.clone());
        self.spawn_merger(repo, service.clone());
        if let Err(error) = service.send(ClientCommand::RequestSessionList).await {
            tracing::warn!(%error, "failed to seed the new daemon's session list");
        }
    }

    /// Unregister a project daemon; its cached sessions leave the merged list.
    pub fn remove_daemon(&self, repo: &Path) {
        let repo = repo.canonicalize().unwrap_or_else(|_| repo.to_path_buf());
        self.daemons.write().unwrap().remove(&repo);
        self.shared.forget_project(&repo);
    }

    /// Create a session on a specific project (`repo` = canonical repository
    /// path; the primary's path selects the in-process runtime).
    pub async fn create_session_for(
        &self,
        repo: &Path,
        request: CreateSessionRequest,
    ) -> Result<SessionBootstrap, ClientError> {
        let repo = repo.canonicalize().unwrap_or_else(|_| repo.to_path_buf());
        let service = if repo == self.primary_repo {
            self.primary.clone()
        } else {
            self.daemons
                .read()
                .unwrap()
                .get(&repo)
                .cloned()
                .ok_or_else(|| {
                    ClientError::Runtime(format!("unknown project: {}", repo.display()))
                })?
        };
        let bootstrap = service.create_session(request).await?;
        self.shared
            .sessions
            .write()
            .unwrap()
            .insert(bootstrap.session.id.clone(), repo);
        Ok(bootstrap)
    }

    /// The service owning `session`, defaulting to the primary when ownership
    /// is unknown (a session never listed nor snapshotted is almost always a
    /// primary one — and the error surfaces fast if not).
    fn service_for_session(&self, session_id: &SessionId) -> Arc<dyn LocalRuntimeService> {
        let owner = self
            .shared
            .sessions
            .read()
            .unwrap()
            .get(session_id)
            .cloned();
        match owner {
            Some(repo) if repo != self.primary_repo => self
                .daemons
                .read()
                .unwrap()
                .get(&repo)
                .cloned()
                .unwrap_or_else(|| self.primary.clone()),
            _ => self.primary.clone(),
        }
    }

    /// Merge one daemon's global stream into the shared one: cache every
    /// `SessionList`, forward `RuntimeReady`, drop everything else (session
    /// events flow through `subscribe_session` instead).
    fn spawn_merger(&self, repo: PathBuf, service: Arc<dyn LocalRuntimeService>) {
        let mut events = service.subscribe();
        let shared = self.shared.clone();
        let task = tokio::spawn(async move {
            loop {
                match events.recv().await {
                    Ok(RuntimeEvent::SessionList { sessions }) => {
                        shared.apply_session_list(&repo, sessions);
                    }
                    Ok(event @ RuntimeEvent::RuntimeReady) => {
                        let _ = shared.merged.send(event);
                    }
                    Ok(_) => {}
                    Err(broadcast::error::RecvError::Lagged(skipped)) => {
                        tracing::warn!(skipped, repo = %repo.display(), "daemon event merger lagged");
                    }
                    Err(broadcast::error::RecvError::Closed) => return,
                }
            }
        });
        self.mergers.lock().unwrap().push(task);
    }

    /// Fan a session-less command out to every daemon, tolerating individual
    /// failures (a dead daemon must not blackhole the others' answers).
    async fn fan_out(&self, command: ClientCommand) -> Result<(), ClientError> {
        self.primary.send(command.clone()).await?;
        let daemons: Vec<(PathBuf, Arc<dyn LocalRuntimeService>)> = self
            .daemons
            .read()
            .unwrap()
            .iter()
            .map(|(repo, service)| (repo.clone(), service.clone()))
            .collect();
        for (repo, daemon) in daemons {
            if let Err(error) = daemon.send(command.clone()).await {
                tracing::warn!(%error, repo = %repo.display(), "daemon failed a broadcast command");
            }
        }
        Ok(())
    }

    /// Route one command envelope to its owning daemon. Session-less commands
    /// key off the envelope's session target (set by the WS layer from the
    /// frame), so an approval decision reaches the daemon whose session raised
    /// the request.
    async fn route_envelope(&self, envelope: CommandEnvelope) -> Result<(), ClientError> {
        if matches!(&envelope.command, ClientCommand::RequestSessionList) {
            return self.fan_out(envelope.command).await;
        }
        if matches!(&envelope.command, ClientCommand::Quit) {
            return self.primary.send(envelope.command).await;
        }
        let target = envelope
            .command
            .session_id()
            .cloned()
            .unwrap_or_else(|| envelope.session_id.clone());
        self.service_for_session(&target).deliver(envelope).await
    }
}

#[async_trait]
impl InteractiveRuntimeClient for RouterService {
    async fn send(&self, command: ClientCommand) -> Result<(), ClientError> {
        match &command {
            ClientCommand::RequestSessionList => self.fan_out(command).await,
            ClientCommand::Quit => self.primary.send(command).await,
            _ => {
                let target = command.session_id().cloned();
                match target {
                    Some(session_id) => self.service_for_session(&session_id).send(command).await,
                    // Raw `send` carries no envelope target for session-less
                    // commands (approval decisions); the primary is the best
                    // available guess. The WS path uses `deliver`, which
                    // routes these correctly.
                    None => self.primary.send(command).await,
                }
            }
        }
    }

    async fn deliver(&self, envelope: CommandEnvelope) -> Result<(), ClientError> {
        self.route_envelope(envelope).await
    }

    /// The merged cross-project stream (see the module docs).
    fn subscribe(&self) -> broadcast::Receiver<RuntimeEvent> {
        self.shared.merged.subscribe()
    }

    /// Delegate to the owning daemon's per-session stream.
    fn subscribe_session(&self, session_id: &SessionId) -> broadcast::Receiver<RuntimeEvent> {
        self.service_for_session(session_id)
            .subscribe_session(session_id)
    }

    async fn snapshot(&self, session_id: &SessionId) -> Result<UiSessionSnapshot, ClientError> {
        // Known ownership: go straight to the owner.
        let owner = self
            .shared
            .sessions
            .read()
            .unwrap()
            .get(session_id)
            .cloned();
        if let Some(repo) = owner {
            return match self
                .service_for_session(session_id)
                .snapshot(session_id)
                .await
            {
                Ok(snapshot) => Ok(snapshot),
                // The daemon may have restarted without its state; fall
                // through to a full probe before giving up.
                Err(_) => self.probe_snapshot(session_id, Some(&repo)).await,
            };
        }
        self.probe_snapshot(session_id, None).await
    }
}

impl RouterService {
    /// Try the primary, then every daemon; backfill ownership on the first
    /// hit. `skip` avoids re-probing a known owner that just failed.
    async fn probe_snapshot(
        &self,
        session_id: &SessionId,
        skip: Option<&Path>,
    ) -> Result<UiSessionSnapshot, ClientError> {
        if skip != Some(self.primary_repo.as_path())
            && let Ok(snapshot) = self.primary.snapshot(session_id).await
        {
            self.shared
                .sessions
                .write()
                .unwrap()
                .insert(session_id.clone(), self.primary_repo.clone());
            return Ok(snapshot);
        }
        let daemons: Vec<(PathBuf, Arc<dyn LocalRuntimeService>)> = self
            .daemons
            .read()
            .unwrap()
            .iter()
            .map(|(repo, service)| (repo.clone(), service.clone()))
            .collect();
        for (repo, daemon) in daemons {
            if skip == Some(repo.as_path()) {
                continue;
            }
            if let Ok(snapshot) = daemon.snapshot(session_id).await {
                self.shared
                    .sessions
                    .write()
                    .unwrap()
                    .insert(session_id.clone(), repo);
                return Ok(snapshot);
            }
        }
        Err(ClientError::SessionNotFound(session_id.clone()))
    }
}

#[async_trait]
impl LocalRuntimeService for RouterService {
    /// Session creation without an explicit project always lands on the
    /// primary (the repository the WebUI was started for).
    async fn create_session(
        &self,
        request: CreateSessionRequest,
    ) -> Result<SessionBootstrap, ClientError> {
        let bootstrap = self.primary.create_session(request).await?;
        self.shared
            .sessions
            .write()
            .unwrap()
            .insert(bootstrap.session.id.clone(), self.primary_repo.clone());
        Ok(bootstrap)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex as StdMutex;

    use leveler_client_protocol::{ApprovalDecision, CommandId, ProtocolEnvelope};
    use leveler_core::{ApprovalId, now};

    /// A scriptable daemon stand-in: answers snapshots for a fixed session
    /// set, records every command, and lets the test push events.
    struct FakeDaemon {
        known: Vec<SessionId>,
        commands: StdMutex<Vec<ClientCommand>>,
        events: broadcast::Sender<RuntimeEvent>,
    }

    impl FakeDaemon {
        fn new(known: &[&str]) -> Arc<Self> {
            Arc::new(Self {
                known: known.iter().map(|id| SessionId::new(*id)).collect(),
                commands: StdMutex::new(Vec::new()),
                events: broadcast::channel(64).0,
            })
        }

        fn commands(&self) -> Vec<ClientCommand> {
            self.commands.lock().unwrap().clone()
        }

        fn emit_list(&self, sessions: Vec<UiSessionSummary>) {
            let _ = self.events.send(RuntimeEvent::SessionList { sessions });
        }

        fn summary(id: &str, updated_at: &str) -> UiSessionSummary {
            UiSessionSummary {
                id: SessionId::new(id),
                goal: format!("goal {id}"),
                status: "idle".to_string(),
                model: "mock/m".to_string(),
                updated_at: updated_at.to_string(),
                repository: None,
            }
        }
    }

    #[async_trait]
    impl InteractiveRuntimeClient for FakeDaemon {
        async fn send(&self, command: ClientCommand) -> Result<(), ClientError> {
            self.commands.lock().unwrap().push(command);
            Ok(())
        }

        fn subscribe(&self) -> broadcast::Receiver<RuntimeEvent> {
            self.events.subscribe()
        }

        fn subscribe_session(&self, _session_id: &SessionId) -> broadcast::Receiver<RuntimeEvent> {
            self.events.subscribe()
        }

        async fn snapshot(&self, session_id: &SessionId) -> Result<UiSessionSnapshot, ClientError> {
            if !self.known.contains(session_id) {
                return Err(ClientError::SessionNotFound(session_id.clone()));
            }
            Ok(UiSessionSnapshot {
                id: session_id.clone(),
                repository: "/repo".to_string(),
                goal: "g".to_string(),
                model: None,
                mode: leveler_client_protocol::PermissionProfile::Assisted,
                branch: None,
                status: "idle".to_string(),
                messages: Vec::new(),
                pending_interactions: Vec::new(),
                available_models: Vec::new(),
                vision: false,
                last_sequence: None,
                active_tools: Vec::new(),
                plan: None,
                verification: None,
                diff: None,
                checkpoints: Vec::new(),
                completion_report: None,
            })
        }
    }

    #[async_trait]
    impl LocalRuntimeService for FakeDaemon {
        async fn create_session(
            &self,
            _request: CreateSessionRequest,
        ) -> Result<SessionBootstrap, ClientError> {
            let snapshot = self.snapshot(&self.known[0]).await?;
            Ok(SessionBootstrap {
                session: snapshot,
                context_window: 4096,
            })
        }
    }

    fn envelope(session_id: &str, command: ClientCommand) -> CommandEnvelope {
        CommandEnvelope {
            command_id: CommandId::generate(),
            session_id: SessionId::new(session_id),
            expected_version: None,
            issued_at: now().to_rfc3339(),
            command,
        }
    }

    /// Receive one SessionList from the merged stream.
    async fn next_merged_list(
        merged: &mut broadcast::Receiver<RuntimeEvent>,
    ) -> Vec<UiSessionSummary> {
        loop {
            match tokio::time::timeout(std::time::Duration::from_secs(5), merged.recv()).await {
                Ok(Ok(RuntimeEvent::SessionList { sessions })) => return sessions,
                Ok(Ok(_)) => continue,
                other => panic!("expected a merged session list, got {other:?}"),
            }
        }
    }

    #[tokio::test]
    async fn snapshot_probes_and_backfills_ownership() {
        let primary = FakeDaemon::new(&["p1"]);
        let daemon = FakeDaemon::new(&["d1"]);
        let router = RouterService::new(primary.clone(), PathBuf::from("/primary"));
        router
            .add_daemon(PathBuf::from("/daemon"), daemon.clone())
            .await;

        // Unknown session: probed, found on the daemon, mapping backfilled.
        let snapshot = router.snapshot(&SessionId::new("d1")).await.unwrap();
        assert_eq!(snapshot.id.as_str(), "d1");
        // A second lookup goes straight to the owner (primary not re-probed,
        // which a panic-on-repeat fake would catch — here the mapping itself
        // is the assertion).
        assert!(matches!(
            router.snapshot(&SessionId::new("d1")).await,
            Ok(snapshot) if snapshot.id.as_str() == "d1"
        ));
        // Truly unknown → 404-shaped error.
        assert!(matches!(
            router.snapshot(&SessionId::new("nope")).await,
            Err(ClientError::SessionNotFound(_))
        ));
    }

    #[tokio::test]
    async fn commands_route_to_the_owning_daemon() {
        let primary = FakeDaemon::new(&["p1"]);
        let daemon = FakeDaemon::new(&["d1"]);
        let router = RouterService::new(primary.clone(), PathBuf::from("/primary"));
        router
            .add_daemon(PathBuf::from("/daemon"), daemon.clone())
            .await;
        // Teach ownership through a snapshot probe.
        router.snapshot(&SessionId::new("d1")).await.unwrap();

        let command = ClientCommand::SubmitMessage {
            session_id: SessionId::new("d1"),
            content: "hi".to_string(),
            attachments: Vec::new(),
        };
        router
            .deliver_protocol(ProtocolEnvelope::wrap(envelope("d1", command.clone())))
            .await
            .unwrap();
        assert!(daemon.commands().contains(&command));
        assert!(!primary.commands().contains(&command));

        // Session-less approval decisions route by the envelope's session.
        let approval = ClientCommand::ApprovalDecision {
            request_id: ApprovalId::new("a1"),
            decision: ApprovalDecision::ApproveOnce,
        };
        router
            .deliver_protocol(ProtocolEnvelope::wrap(envelope("d1", approval.clone())))
            .await
            .unwrap();
        assert!(daemon.commands().contains(&approval));
        assert!(!primary.commands().contains(&approval));

        // Quit only ever reaches the primary.
        router.send(ClientCommand::Quit).await.unwrap();
        assert!(primary.commands().contains(&ClientCommand::Quit));
        assert!(!daemon.commands().contains(&ClientCommand::Quit));
    }

    #[tokio::test]
    async fn session_lists_fan_out_and_merge() {
        let primary = FakeDaemon::new(&["p1"]);
        let daemon = FakeDaemon::new(&["d1"]);
        let router = RouterService::new(primary.clone(), PathBuf::from("/primary"));
        let mut merged = router.subscribe();
        router
            .add_daemon(PathBuf::from("/daemon"), daemon.clone())
            .await;

        // add_daemon seeded one RequestSessionList on the daemon already.
        assert!(
            daemon
                .commands()
                .contains(&ClientCommand::RequestSessionList)
        );

        router
            .send(ClientCommand::RequestSessionList)
            .await
            .unwrap();
        assert_eq!(
            primary
                .commands()
                .iter()
                .filter(|c| matches!(c, ClientCommand::RequestSessionList))
                .count(),
            1
        );
        assert_eq!(
            daemon
                .commands()
                .iter()
                .filter(|c| matches!(c, ClientCommand::RequestSessionList))
                .count(),
            2
        );

        // Each daemon answers on its own stream; the merged broadcast carries
        // the union, newest first.
        primary.emit_list(vec![FakeDaemon::summary("p1", "2026-01-01T00:00:00Z")]);
        let list = next_merged_list(&mut merged).await;
        assert_eq!(list.len(), 1);
        daemon.emit_list(vec![FakeDaemon::summary("d1", "2026-02-01T00:00:00Z")]);
        let list = next_merged_list(&mut merged).await;
        assert_eq!(
            list.iter().map(|s| s.id.as_str()).collect::<Vec<_>>(),
            ["d1", "p1"]
        );
        // The router stamps each summary with its owning repository so the
        // frontend can group the merged list by project.
        assert_eq!(list[0].repository.as_deref(), Some("/daemon"));
        assert_eq!(list[1].repository.as_deref(), Some("/primary"));

        // Ownership learned from lists: d1 now routes to the daemon without a
        // snapshot probe (primary never saw d1, yet routing still works).
        let command = ClientCommand::CancelCurrentTurn {
            session_id: SessionId::new("d1"),
        };
        router.send(command.clone()).await.unwrap();
        assert!(daemon.commands().contains(&command));
    }

    #[tokio::test]
    async fn subscribe_session_delegates_to_the_owner() {
        let primary = FakeDaemon::new(&["p1"]);
        let daemon = FakeDaemon::new(&["d1"]);
        let router = RouterService::new(primary.clone(), PathBuf::from("/primary"));
        router
            .add_daemon(PathBuf::from("/daemon"), daemon.clone())
            .await;
        router.snapshot(&SessionId::new("d1")).await.unwrap();

        let mut events = router.subscribe_session(&SessionId::new("d1"));
        let _ = daemon.events.send(RuntimeEvent::Notification {
            level: leveler_client_protocol::NotificationLevel::Info,
            message: "daemon event".to_string(),
        });
        let event = tokio::time::timeout(std::time::Duration::from_secs(5), events.recv())
            .await
            .unwrap()
            .unwrap();
        assert!(matches!(event, RuntimeEvent::Notification { .. }));

        // Unknown ownership falls back to the primary's stream.
        let mut fallback = router.subscribe_session(&SessionId::new("unknown"));
        let _ = primary.events.send(RuntimeEvent::Notification {
            level: leveler_client_protocol::NotificationLevel::Info,
            message: "primary event".to_string(),
        });
        let event = tokio::time::timeout(std::time::Duration::from_secs(5), fallback.recv())
            .await
            .unwrap()
            .unwrap();
        assert!(matches!(event, RuntimeEvent::Notification { .. }));
    }

    #[tokio::test]
    async fn create_session_for_targets_the_named_project() {
        let primary = FakeDaemon::new(&["p1"]);
        let daemon = FakeDaemon::new(&["d1"]);
        let router = RouterService::new(primary.clone(), PathBuf::from("/primary"));
        router
            .add_daemon(PathBuf::from("/daemon"), daemon.clone())
            .await;
        let request = CreateSessionRequest {
            goal: "g".to_string(),
            model: None,
            mode: leveler_client_protocol::PermissionProfile::Assisted,
        };
        let bootstrap = router
            .create_session_for(Path::new("/daemon"), request.clone())
            .await
            .unwrap();
        assert_eq!(bootstrap.session.id.as_str(), "d1");
        // Session-targeted commands for the new session route correctly now.
        let command = ClientCommand::CancelCurrentTurn {
            session_id: SessionId::new("d1"),
        };
        router.send(command.clone()).await.unwrap();
        assert!(daemon.commands().contains(&command));

        let bootstrap = router.create_session(request).await.unwrap();
        assert_eq!(bootstrap.session.id.as_str(), "p1");

        assert!(matches!(
            router
                .create_session_for(Path::new("/missing"), CreateSessionRequest {
                    goal: "g".to_string(),
                    model: None,
                    mode: leveler_client_protocol::PermissionProfile::Assisted,
                })
                .await,
            Err(ClientError::Runtime(message)) if message.contains("unknown project")
        ));
    }
}
