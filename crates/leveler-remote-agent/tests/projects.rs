//! One pairing, several open projects — and no crossing between them.
//!
//! The failure this guards against is not "the feature is missing"; it is a
//! phone showing project A's screen while its commands land in project B. Every
//! positive assertion here is paired with the negative one that makes it mean
//! something: the *other* project received nothing.

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use leveler_client_protocol::{
    ClientCommand, ClientError, InteractiveRuntimeClient, PermissionProfile, RuntimeEvent,
    SessionId, UiSessionSnapshot,
};
use leveler_local_transport::{
    CreateSessionRequest, LocalRuntimeService, LocalSocketServer, SessionBootstrap,
};
use leveler_remote_agent::{
    AgentBridge, ProjectInfo, ProjectRouter, ProjectRoutes, RouteError, TrustedDevices,
};
use leveler_remote_protocol::pairing::PairingScope;
use leveler_remote_protocol::tunnel::{RpcMethod, RpcRequestPayload, rpc_stream_id};
use leveler_remote_protocol::{
    ContentType, Sender, SignedEnvelope, SigningKey, VerifyParams, VerifyingKey,
};
use leveler_session_wire::ProjectStatus;
use tokio::sync::broadcast;
use tokio_util::sync::CancellationToken;

const DEVICE_SEED: [u8; 32] = [81u8; 32];
const RUNTIME_SEED: [u8; 32] = [82u8; 32];
const RUNTIME_ID: &str = "rt_host";
const DEVICE_ID: &str = "dev_phone";
const AT: &str = "2026-07-25T12:00:00Z";
const ALPHA: &str = "aaaaaaaaaaaaaaaa";
const BETA: &str = "bbbbbbbbbbbbbbbb";

fn device_key() -> SigningKey {
    SigningKey::from_seed(&DEVICE_SEED).unwrap()
}

fn runtime_key() -> SigningKey {
    SigningKey::from_seed(&RUNTIME_SEED).unwrap()
}

/// A runtime that only records, so a test can assert what did *not* arrive.
#[derive(Default)]
struct RecordingRuntime {
    label: String,
    delivered: Arc<Mutex<Vec<ClientCommand>>>,
}

#[async_trait]
impl InteractiveRuntimeClient for RecordingRuntime {
    async fn send(&self, command: ClientCommand) -> Result<(), ClientError> {
        self.delivered.lock().unwrap().push(command);
        Ok(())
    }

    fn subscribe(&self) -> broadcast::Receiver<RuntimeEvent> {
        broadcast::channel(1).1
    }

    async fn snapshot(&self, session_id: &SessionId) -> Result<UiSessionSnapshot, ClientError> {
        Ok(UiSessionSnapshot {
            id: session_id.clone(),
            // The label rides along so a test can tell which project answered.
            repository: self.label.clone(),
            goal: String::new(),
            model: None,
            mode: PermissionProfile::Assisted,
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
            user_shells: Vec::new(),
            completion_report: None,
        })
    }
}

#[async_trait]
impl LocalRuntimeService for RecordingRuntime {
    async fn create_session(
        &self,
        request: CreateSessionRequest,
    ) -> Result<SessionBootstrap, ClientError> {
        let mut session = self.snapshot(&SessionId::new("new")).await?;
        session.goal = request.goal;
        Ok(SessionBootstrap {
            session,
            context_window: 128_000,
        })
    }
}

/// Two projects, one of which can be taken offline mid-test.
struct TwoProjects {
    alpha: Arc<RecordingRuntime>,
    beta: Arc<RecordingRuntime>,
    beta_online: Arc<Mutex<bool>>,
}

#[async_trait]
impl ProjectRoutes for TwoProjects {
    async fn projects(&self) -> Vec<ProjectInfo> {
        vec![
            ProjectInfo {
                project_id: ALPHA.to_string(),
                path_display: "alpha".to_string(),
                status: ProjectStatus::Online,
            },
            ProjectInfo {
                project_id: BETA.to_string(),
                path_display: "beta".to_string(),
                status: if *self.beta_online.lock().unwrap() {
                    ProjectStatus::Online
                } else {
                    ProjectStatus::Offline
                },
            },
        ]
    }

    async fn runtime(&self, project_id: &str) -> Result<Arc<dyn LocalRuntimeService>, RouteError> {
        match project_id {
            ALPHA => Ok(self.alpha.clone()),
            BETA if *self.beta_online.lock().unwrap() => Ok(self.beta.clone()),
            BETA => Err(RouteError::ProjectOffline),
            _ => Err(RouteError::UnknownProject),
        }
    }

    async fn implied_project(&self) -> Result<String, RouteError> {
        Err(RouteError::ProjectRequired)
    }
}

struct Fixture {
    bridge: AgentBridge,
    alpha: Arc<Mutex<Vec<ClientCommand>>>,
    beta: Arc<Mutex<Vec<ClientCommand>>>,
    beta_online: Arc<Mutex<bool>>,
}

fn fixture(dir: &tempfile::TempDir) -> Fixture {
    let mut devices = TrustedDevices::load(dir.path().join("remote/devices.json")).unwrap();
    devices
        .accept(
            DEVICE_ID,
            &device_key().verifying_key(),
            "iPhone",
            PairingScope::Interactive,
            AT,
        )
        .unwrap();

    let alpha = Arc::new(RecordingRuntime {
        label: "alpha".to_string(),
        delivered: Arc::new(Mutex::new(Vec::new())),
    });
    let beta = Arc::new(RecordingRuntime {
        label: "beta".to_string(),
        delivered: Arc::new(Mutex::new(Vec::new())),
    });
    let beta_online = Arc::new(Mutex::new(true));
    let routes = Arc::new(TwoProjects {
        alpha: alpha.clone(),
        beta: beta.clone(),
        beta_online: beta_online.clone(),
    });

    Fixture {
        bridge: AgentBridge::new(routes, devices, RUNTIME_ID, runtime_key(), false),
        alpha: alpha.delivered.clone(),
        beta: beta.delivered.clone(),
        beta_online,
    }
}

fn deliver_frame(session_id: &str, content: &str, seq: u64) -> SignedEnvelope {
    let body = serde_json::json!({
        "type": "deliver",
        "command_id": format!("cmd-{seq}"),
        "session_id": session_id,
        "command": {
            "type": "submit_message",
            "session_id": session_id,
            "content": content,
            "attachments": []
        }
    })
    .to_string();
    SignedEnvelope::sign(
        &device_key(),
        Sender::Device,
        DEVICE_ID,
        RUNTIME_ID,
        "str_app",
        seq,
        AT,
        ContentType::SessionUpstream,
        body.as_bytes(),
    )
    .unwrap()
}

fn rpc_frame(
    method: RpcMethod,
    project_id: Option<&str>,
    body: serde_json::Value,
    uuid: &str,
) -> SignedEnvelope {
    let payload = serde_json::to_vec(&RpcRequestPayload {
        method,
        project_id: project_id.map(|id| id.to_string()),
        body,
    })
    .unwrap();
    SignedEnvelope::sign(
        &device_key(),
        Sender::Device,
        DEVICE_ID,
        RUNTIME_ID,
        &rpc_stream_id(uuid),
        1,
        AT,
        ContentType::RpcRequest,
        &payload,
    )
    .unwrap()
}

/// Verify a response the way the phone does, and return its payload.
fn verified(envelope: &SignedEnvelope, anchored: &VerifyingKey) -> serde_json::Value {
    let payload = envelope
        .verify(&VerifyParams {
            expected_recipient_id: DEVICE_ID,
            public_key: anchored,
            now: AT,
        })
        .expect("the phone must be able to verify this");
    serde_json::from_slice(&payload).unwrap()
}

/// The property the whole feature turns on.
#[tokio::test]
async fn each_projects_commands_reach_only_that_project() {
    let dir = tempfile::tempdir().unwrap();
    let fixture = fixture(&dir);

    fixture
        .bridge
        .admit_upstream(ALPHA, &deliver_frame("s-alpha", "给 alpha", 1), AT, None)
        .await
        .expect("alpha accepts its own frame");

    assert_eq!(fixture.alpha.lock().unwrap().len(), 1);
    assert!(
        fixture.beta.lock().unwrap().is_empty(),
        "the other project must not see it"
    );

    fixture
        .bridge
        .admit_upstream(BETA, &deliver_frame("s-beta", "给 beta", 2), AT, None)
        .await
        .expect("beta accepts its own frame");

    assert_eq!(fixture.alpha.lock().unwrap().len(), 1, "alpha unchanged");
    assert_eq!(fixture.beta.lock().unwrap().len(), 1);
}

/// A snapshot is answered by the project the stream is bound to.
#[tokio::test]
async fn a_snapshot_comes_from_the_streams_own_project() {
    let dir = tempfile::tempdir().unwrap();
    let fixture = fixture(&dir);

    let alpha = fixture
        .bridge
        .snapshot(ALPHA, &SessionId::new("s1"))
        .await
        .unwrap();
    let beta = fixture
        .bridge
        .snapshot(BETA, &SessionId::new("s1"))
        .await
        .unwrap();
    assert_eq!(alpha.repository, "alpha");
    assert_eq!(beta.repository, "beta");
}

/// One project going offline must not take the others with it.
#[tokio::test]
async fn an_offline_project_is_isolated_from_the_rest() {
    let dir = tempfile::tempdir().unwrap();
    let fixture = fixture(&dir);
    *fixture.beta_online.lock().unwrap() = false;

    let error = fixture
        .bridge
        .admit_upstream(BETA, &deliver_frame("s-beta", "给 beta", 1), AT, None)
        .await
        .expect_err("an offline project cannot take a command");
    assert_eq!(error.code(), "project_offline");
    assert!(fixture.beta.lock().unwrap().is_empty());

    fixture
        .bridge
        .admit_upstream(ALPHA, &deliver_frame("s-alpha", "给 alpha", 2), AT, None)
        .await
        .expect("the healthy project keeps working");
    assert_eq!(fixture.alpha.lock().unwrap().len(), 1);

    // And it is still listed, greyed out rather than gone.
    let listed = fixture.bridge.projects().await;
    assert_eq!(listed.len(), 2);
    assert_eq!(listed[1].status, ProjectStatus::Offline);
}

/// A project id the host does not have is refused, not routed to a default.
#[tokio::test]
async fn an_unknown_project_delivers_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let fixture = fixture(&dir);

    let error = fixture
        .bridge
        .admit_upstream(
            "cccccccccccccccc",
            &deliver_frame("s1", "无处可去", 1),
            AT,
            None,
        )
        .await
        .expect_err("an unknown project has no runtime");
    assert_eq!(error.code(), "unknown_project");
    assert!(fixture.alpha.lock().unwrap().is_empty());
    assert!(fixture.beta.lock().unwrap().is_empty());
}

/// With several projects open, an unnamed target is an error rather than a
/// guess — guessing would deliver to the wrong repository.
#[tokio::test]
async fn an_unnamed_project_is_refused_when_several_are_open() {
    let dir = tempfile::tempdir().unwrap();
    let fixture = fixture(&dir);

    let error = fixture
        .bridge
        .resolve_project(None)
        .await
        .expect_err("ambiguous");
    assert_eq!(error.code(), "project_required");
    assert_eq!(
        fixture.bridge.resolve_project(Some(BETA)).await.unwrap(),
        BETA
    );
}

/// The phone's project list arrives inside a runtime-signed envelope, so a relay
/// cannot invent a project or hide one.
#[tokio::test]
async fn the_project_list_is_signed_by_the_runtime() {
    let dir = tempfile::tempdir().unwrap();
    let fixture = fixture(&dir);
    *fixture.beta_online.lock().unwrap() = false;

    let response = fixture
        .bridge
        .handle_rpc(
            &rpc_frame(RpcMethod::ListProjects, None, serde_json::json!({}), "aaaa"),
            AT,
            None,
        )
        .await
        .unwrap();

    let listed = verified(&response, &runtime_key().verifying_key());
    let listed = listed.as_array().unwrap();
    assert_eq!(listed.len(), 2);
    assert_eq!(listed[0]["project_id"], ALPHA);
    assert_eq!(listed[0]["path_display"], "alpha");
    assert_eq!(listed[0]["status"], "online");
    assert_eq!(listed[1]["status"], "offline");
}

/// `create_session` takes its project from the signed payload. A relay routes
/// the request but does not choose where a session is created.
#[tokio::test]
async fn create_session_goes_to_the_project_the_device_signed_for() {
    let dir = tempfile::tempdir().unwrap();
    let fixture = fixture(&dir);

    let response = fixture
        .bridge
        .handle_rpc(
            &rpc_frame(
                RpcMethod::CreateSession,
                Some(BETA),
                serde_json::json!({"goal": "修个 bug", "model": null, "mode": "assisted"}),
                "bbbb",
            ),
            AT,
            None,
        )
        .await
        .unwrap();

    let bootstrap = verified(&response, &runtime_key().verifying_key());
    assert_eq!(
        bootstrap["session"]["repository"], "beta",
        "the session must be created in the signed-for project"
    );
}

// ------------------------------------------------------- the real router

/// The registry-backed router against actual daemons: two live sockets, one
/// dead entry. This is the shape a developer's machine has, and the reason the
/// project id is derived from the socket name rather than invented.
#[tokio::test]
#[cfg(unix)]
async fn the_router_attaches_to_live_daemons_and_reports_the_rest_offline() {
    let dir = tempfile::tempdir().unwrap();
    let sockets = dir.path().join("sock");
    std::fs::create_dir_all(&sockets).unwrap();

    // Repositories as the registry names them, with a socket per repo keyed by
    // a stand-in hash — the same relationship `Layout::socket_path` produces.
    let alpha_repo = dir.path().join("repos/alpha");
    let beta_repo = dir.path().join("repos/beta");
    let dead_repo = dir.path().join("repos/dead");
    let socket_for = {
        let sockets = sockets.clone();
        move |repo: &std::path::Path| {
            let name = repo.file_name().unwrap().to_string_lossy().to_string();
            sockets.join(format!("{name}.sock"))
        }
    };

    let shutdown = CancellationToken::new();
    for (repo, label) in [(&alpha_repo, "alpha"), (&beta_repo, "beta")] {
        let runtime = Arc::new(RecordingRuntime {
            label: label.to_string(),
            delivered: Arc::new(Mutex::new(Vec::new())),
        });
        let server = LocalSocketServer::bind(socket_for(repo), runtime)
            .await
            .unwrap();
        tokio::spawn(server.serve(shutdown.clone()));
    }

    let registry = dir.path().join("web-projects.json");
    std::fs::write(
        &registry,
        serde_json::json!({
            "projects": [alpha_repo, beta_repo, dead_repo],
            "aliases": {beta_repo.to_string_lossy(): "后端"},
            "ignored": []
        })
        .to_string(),
    )
    .unwrap();

    let router = ProjectRouter::new(registry, socket_for);
    let listed = router.projects().await;

    assert_eq!(listed.len(), 3);
    assert_eq!(
        listed[0].project_id, "alpha",
        "the id follows the socket name"
    );
    assert_eq!(listed[0].status, ProjectStatus::Online);
    assert_eq!(listed[1].path_display, "后端", "the user's alias is shown");
    assert_eq!(listed[1].status, ProjectStatus::Online);
    assert_eq!(
        listed[2].status,
        ProjectStatus::Offline,
        "a registry entry with no daemon is offline, not missing"
    );

    // A live one resolves to a runtime; the dead one does not.
    assert!(router.runtime("alpha").await.is_ok());
    assert_eq!(
        router.runtime("dead").await.err(),
        Some(RouteError::ProjectOffline)
    );
    assert_eq!(
        router.runtime("nope").await.err(),
        Some(RouteError::UnknownProject)
    );
    assert_eq!(
        router.implied_project().await.unwrap_err(),
        RouteError::ProjectRequired,
        "three open projects means the caller must say which"
    );

    // A daemon that stops *after* the router attached to it must stop being
    // reported online. The attachment is cached — deliberately, so a frame does
    // not pay for a reconnect — and a cached handle was answering the status
    // question too, so a project the user had closed went on being offered by
    // the phone for as long as the agent lived.
    shutdown.cancel();
    for _ in 0..50 {
        if router
            .projects()
            .await
            .iter()
            .all(|project| project.status == ProjectStatus::Offline)
        {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    panic!(
        "daemons stopped, but the router still calls them online: {:?}",
        router.projects().await
    );
}

/// A repository the user removed must not come back through the remote door.
#[tokio::test]
#[cfg(unix)]
async fn a_removed_repository_is_not_listed() {
    let dir = tempfile::tempdir().unwrap();
    let repo = dir.path().join("repos/alpha");
    let registry = dir.path().join("web-projects.json");
    std::fs::write(
        &registry,
        serde_json::json!({
            "projects": [&repo],
            "ignored": [&repo]
        })
        .to_string(),
    )
    .unwrap();

    let router = ProjectRouter::new(registry, |repo: &std::path::Path| {
        repo.with_extension("sock")
    });
    assert!(router.projects().await.is_empty());
}
