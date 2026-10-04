//! Real Windows Host/serve acceptance. Run on Windows; cross-check is compilation only.
#![cfg(windows)]

use leveler_client_protocol::{ApprovalPolicy, InteractiveRuntimeClient, PermissionProfile};
use leveler_local_transport::{
    CreateSessionRequest, LocalRuntimeService, LocalSocketRuntimeClient,
};
use leveler_project::Layout;
use leveler_runtime_host::{
    DaemonReviver, DetachedRuntimeLaunch, HandoffAction, HandoffEvent, HandoffUi,
    ensure_default_runtime, probe_default_runtime,
};
use leveler_test_support::ManagedChild;
use std::{
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::Arc,
    time::{Duration, Instant},
};

struct Quiet;
impl HandoffUi for Quiet {
    fn emit(&self, _: HandoffEvent) {}
    fn input(&self) -> Option<tokio::sync::mpsc::UnboundedReceiver<HandoffAction>> {
        None
    }
}
struct Daemons(Vec<u32>);
impl Drop for Daemons {
    fn drop(&mut self) {
        for pid in &self.0 {
            let _ = kill(*pid);
        }
    }
}
fn kill(pid: u32) -> bool {
    Command::new("taskkill")
        .args(["/PID", &pid.to_string(), "/T", "/F"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

#[test]
fn windows_real_host_lifecycle_and_ownership() {
    if let Ok(root) = std::env::var("LEVELER_WINDOWS_HOST_TEST_ROOT") {
        tokio::runtime::Runtime::new()
            .unwrap()
            .block_on(run(Path::new(&root)));
        return;
    }
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    for dir in ["home", "repo", "configs/providers", "configs/models"] {
        std::fs::create_dir_all(root.join(dir)).unwrap();
    }
    std::fs::write(
        root.join("configs/providers/mock.yaml"),
        "id: mock\nprotocol: openai_chat\nbase_url: http://127.0.0.1:9\n",
    )
    .unwrap();
    std::fs::write(root.join("configs/models/m.yaml"), MODEL).unwrap();
    // Process-local environment isolation without mutating the shared test runner.
    let mut command = Command::new(std::env::current_exe().unwrap());
    command
        .args([
            "--exact",
            "windows_real_host_lifecycle_and_ownership",
            "--nocapture",
        ])
        .env("LEVELER_WINDOWS_HOST_TEST_ROOT", root)
        .env("LEVELER_HOME", root.join("home"))
        .env("LEVELER_CONFIG_DIR", root.join("configs"));
    let mut child = ManagedChild::spawn(&mut command).unwrap();
    let deadline = Instant::now() + Duration::from_secs(90);
    loop {
        if let Some(status) = child.try_wait().unwrap() {
            assert!(
                status.success(),
                "Windows Host acceptance child failed: {status}"
            );
            break;
        }
        assert!(
            Instant::now() < deadline,
            "Windows Host acceptance child timed out"
        );
        std::thread::sleep(Duration::from_millis(25));
    }
}

async fn run(root: &Path) {
    // The same explicit paths the launcher passes to the daemon below, not
    // `Layout::resolve`: this test binary never runs `main`, so the process
    // environment was never installed as CodeLeveler's snapshot and the
    // env-var lookup would silently fall back to `<repo>/configs` — where no
    // mock model exists — and the daemon would exit with "no models configured".
    let layout = Layout::ephemeral(
        root.join("repo"),
        Some(root.join("configs")),
        &root.join("home"),
    );
    let launch = DetachedRuntimeLaunch {
        executable: PathBuf::from(env!("CARGO_BIN_EXE_leveler")),
        ready_prefix: "windows-host-contract".into(),
    };
    let ui: Arc<dyn HandoffUi> = Arc::new(Quiet);
    let mut guard = Daemons(Vec::new());
    assert!(
        probe_default_runtime(&layout.socket_path())
            .await
            .unwrap()
            .is_none()
    );
    let client = ensure_default_runtime(&layout, &launch, ui.clone())
        .await
        .unwrap();
    let first = LocalRuntimeService::runtime_info(&client).await.unwrap();
    guard.0.push(first.pid);
    assert_ne!(first.pid, std::process::id());
    let session = client
        .create_session(CreateSessionRequest {
            workspace: leveler_local_transport::CreateWorkspaceSelection::RuntimeDefault,
            goal: "Windows Host contract".into(),
            model: None,
            mode: PermissionProfile::Assisted,
            approval_policy: ApprovalPolicy::Interactive,
        })
        .await
        .unwrap()
        .session
        .id;
    assert_eq!(client.snapshot(&session).await.unwrap().id, session);
    let adopt_only = DetachedRuntimeLaunch {
        executable: root.join("missing-leveler.exe"),
        ready_prefix: "must-not-spawn".into(),
    };
    let adopted = ensure_default_runtime(&layout, &adopt_only, ui.clone())
        .await
        .unwrap();
    assert_eq!(
        LocalRuntimeService::runtime_info(&adopted)
            .await
            .unwrap()
            .pid,
        first.pid
    );
    drop(adopted);
    assert!(kill(first.pid));
    guard.0.retain(|pid| *pid != first.pid);
    wait_absent(&layout).await;
    client.set_reviver(Arc::new(DaemonReviver::new(
        layout.clone(),
        launch.clone(),
        ui.clone(),
    )));
    let revived = LocalRuntimeService::runtime_info(&client).await.unwrap();
    guard.0.push(revived.pid);
    assert_ne!(revived.pid, first.pid);
    assert_eq!(revived.runtime_id, first.runtime_id);
    assert_eq!(client.snapshot(&session).await.unwrap().id, session);
    let reconnected = LocalSocketRuntimeClient::connect(layout.socket_path())
        .await
        .unwrap();
    assert_eq!(
        LocalRuntimeService::runtime_info(&reconnected)
            .await
            .unwrap()
            .pid,
        revived.pid
    );
    drop(reconnected);
    drop(client);
    // A changed config generation asks the real old daemon to retire before
    // a replacement owns the same endpoint; no fake runtime is involved.
    std::fs::write(
        root.join("configs/providers/mock.yaml"),
        "id: mock\nprotocol: openai_chat\nbase_url: http://127.0.0.1:10\n",
    )
    .unwrap();
    let replacement = ensure_default_runtime(&layout, &launch, ui).await.unwrap();
    // Successful handoff relinquished the old owner; never retain its PID
    // for later cleanup because Windows can recycle process identifiers.
    guard.0.retain(|pid| *pid != revived.pid);
    let info = LocalRuntimeService::runtime_info(&replacement)
        .await
        .unwrap();
    guard.0.push(info.pid);
    assert_ne!(info.pid, revived.pid);
    assert_eq!(info.runtime_id, first.runtime_id);
    assert_eq!(
        info.config_fingerprint.as_deref(),
        Some(
            leveler_app::runtime_config_fingerprint(&layout)
                .unwrap()
                .as_str()
        )
    );
    assert_eq!(replacement.snapshot(&session).await.unwrap().id, session);
    drop(replacement);
    assert!(kill(info.pid));
    guard.0.retain(|pid| *pid != info.pid);
    wait_absent(&layout).await;
    concurrent_owner(root, &layout).await;
}

async fn wait_absent(layout: &Layout) {
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            if probe_default_runtime(&layout.socket_path())
                .await
                .unwrap()
                .is_none()
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    })
    .await
    .expect("terminated owner must stop accepting");
}
async fn concurrent_owner(root: &Path, layout: &Layout) {
    let ready_a = root.join("ready-a.json");
    let ready_b = root.join("ready-b.json");
    let spawn = |ready: &Path| {
        let mut command = Command::new(env!("CARGO_BIN_EXE_leveler"));
        command
            .arg("--repo")
            .arg(
                layout
                    .require_workspace()
                    .expect("this acceptance needs a workspace"),
            )
            .args(["serve", "--ready-json"])
            .arg(ready)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        ManagedChild::spawn(&mut command).unwrap()
    };
    let mut a = spawn(&ready_a);
    let mut b = spawn(&ready_b);
    let deadline = Instant::now() + Duration::from_secs(30);
    let (winner_pid, winner_ready, loser_ready) = loop {
        match (a.try_wait().unwrap(), b.try_wait().unwrap()) {
            (Some(status), None) => {
                assert!(!status.success());
                break (b.id(), &ready_b, &ready_a);
            }
            (None, Some(status)) => {
                assert!(!status.success());
                break (a.id(), &ready_a, &ready_b);
            }
            (Some(_), Some(_)) => panic!("both contenders exited"),
            (None, None) => {
                assert!(Instant::now() < deadline, "no ownership winner");
                tokio::time::sleep(Duration::from_millis(25)).await;
            }
        }
    };
    // std::fs::write publishes the path before the JSON bytes are complete.
    let ready = loop {
        if let Ok(bytes) = std::fs::read(winner_ready)
            && let Ok(value) = serde_json::from_slice::<serde_json::Value>(&bytes)
            && value["pid"].as_u64().is_some_and(|pid| pid > 0)
        {
            break value;
        }
        assert!(
            Instant::now() < deadline,
            "winner did not publish valid readiness JSON"
        );
        tokio::time::sleep(Duration::from_millis(25)).await;
    };
    assert!(!loser_ready.exists(), "loser must not publish readiness");
    assert_eq!(ready["pid"].as_u64(), Some(u64::from(winner_pid)));
    let client = LocalSocketRuntimeClient::connect(layout.socket_path())
        .await
        .unwrap();
    assert_eq!(
        LocalRuntimeService::runtime_info(&client)
            .await
            .unwrap()
            .pid,
        winner_pid
    );
}

const MODEL: &str = r#"
id: m
provider: mock
model_id: mock-model
protocol: openai_chat
capabilities:
  streaming: true
  tool_calling: true
  parallel_tool_calls: false
  structured_output: true
  reasoning: false
  vision: false
limits:
  context_window: 131072
  reliable_context: 65536
  max_output_tokens: 1024
  max_tool_schema_bytes: 8192
  max_parallel_tool_calls: 1
compatibility:
  synthesize_tool_call_ids: true
  drop_unsupported_fields: true
"#;
