//! A lost Execution Host must not make the product unstartable.
//!
//! Reported failure (v1.0.21): the host died while its durable archive still
//! said `Running`. Every start then failed with
//! ``residue kept: the archive still lists a running or killing task``, and the
//! documented `leveler tui --in-process` fallback failed identically because it
//! reconciles the same records.
//!
//! These tests drive the REAL binary over the durable state a crash leaves
//! behind, down both start paths. The residue is planted through the same
//! project layout the binary derives, so the test cannot pass by writing to a
//! different directory than the product reads.

#![cfg(unix)]

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use leveler_core::EnvSnapshot;
use leveler_project::Layout;
use leveler_storage::{Database, SessionRepository};
use leveler_test_support::{ManagedChild, git};

struct TestEnv {
    _tmp: tempfile::TempDir,
    home: PathBuf,
    config_dir: PathBuf,
    repo: PathBuf,
}

fn test_env() -> TestEnv {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("home");
    let repo = tmp.path().join("repo");
    let config_dir = tmp.path().join("configs");
    std::fs::create_dir_all(&home).unwrap();
    std::fs::create_dir_all(&repo).unwrap();
    std::fs::create_dir_all(config_dir.join("providers")).unwrap();
    std::fs::create_dir_all(config_dir.join("models")).unwrap();
    // Auto-update is off so no test reaches the network.
    std::fs::write(
        home.join("config.toml"),
        "default_model = \"mock/m\"\n\n[update]\nauto_update = false\n",
    )
    .unwrap();
    std::fs::write(
        config_dir.join("providers/mock.yaml"),
        "id: mock\nprotocol: openai_chat\nbase_url: http://127.0.0.1:9\n",
    )
    .unwrap();
    std::fs::write(
        config_dir.join("models/m.yaml"),
        r#"
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
"#,
    )
    .unwrap();
    git::init_repo(&repo);
    std::fs::write(repo.join("seed.txt"), "seed\n").unwrap();
    git::run(&repo, &["add", "-A"]);
    git::run(&repo, &["commit", "-qm", "seed"]);
    TestEnv {
        _tmp: tmp,
        home,
        config_dir,
        repo,
    }
}

impl TestEnv {
    /// The same project layout the binary derives from `--repo` + LEVELER_HOME.
    fn layout(&self) -> Layout {
        let environment = EnvSnapshot::new(
            [
                (
                    std::ffi::OsString::from("LEVELER_HOME"),
                    self.home.clone().into(),
                ),
                (std::ffi::OsString::from("HOME"), self.home.clone().into()),
                (
                    std::ffi::OsString::from("LEVELER_CONFIG_DIR"),
                    self.config_dir.clone().into(),
                ),
            ],
            self.repo.clone(),
            self._tmp.path().join("tmp"),
        );
        Layout::ephemeral_with_environment(
            self.repo.clone(),
            Some(self.config_dir.clone()),
            &self.home,
            &environment,
        )
    }

    fn command(&self, args: &[&str]) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_leveler"));
        command
            .arg("--repo")
            .arg(&self.repo)
            .args(args)
            .env("LEVELER_HOME", &self.home)
            .env("LEVELER_CONFIG_DIR", &self.config_dir)
            .stdin(Stdio::null());
        command
    }

    /// Run one launch to completion, capturing stderr in a file this test owns.
    fn run(&self, args: &[&str], timeout: Duration) -> (bool, String) {
        static INVOCATION: AtomicU64 = AtomicU64::new(0);
        let id = INVOCATION.fetch_add(1, Ordering::Relaxed);
        let stderr_path = self.home.join(format!("launch-{id}.stderr.log"));
        let stderr = std::fs::File::create(&stderr_path).unwrap();
        let mut command = self.command(args);
        command.stdout(Stdio::null()).stderr(Stdio::from(stderr));
        let mut child = ManagedChild::spawn(&mut command).expect("spawn leveler");
        let deadline = Instant::now() + timeout;
        while child.try_wait().expect("child status").is_none() {
            assert!(
                Instant::now() < deadline,
                "`leveler {}` never exited within {timeout:?}",
                args.join(" ")
            );
            std::thread::sleep(Duration::from_millis(25));
        }
        let status = child.try_wait().expect("child status").unwrap();
        (
            status.success(),
            std::fs::read_to_string(stderr_path).unwrap(),
        )
    }

    async fn sessions(&self) -> Vec<leveler_storage::SessionRecord> {
        let db = Database::connect(&self.layout().state_dir.join("sessions.db"))
            .await
            .unwrap();
        SessionRepository::new(&db).list().await.unwrap()
    }
}

/// Plant what a crashed host leaves: a ready endpoint nobody listens on, and a
/// live archive record whose process group is really gone.
fn plant_lost_host(env: &TestEnv, task_id: &str) -> PathBuf {
    use std::os::unix::fs::DirBuilderExt;
    let state_dir = env.layout().state_dir.join("execution-host");
    let mut builder = std::fs::DirBuilder::new();
    builder.recursive(true).mode(0o700);
    builder.create(&state_dir).unwrap();
    let dead_pid = exited_group_leader();
    let repo = std::fs::canonicalize(&env.repo).unwrap();

    std::fs::write(
        state_dir.join("ready.json"),
        serde_json::json!({
            "address": "127.0.0.1:1",
            "token": "0".repeat(64),
            "instance": "lost-instance",
            // The recorded pid is dead: nothing owns this endpoint.
            "pid": dead_pid,
            "repo": repo,
            "major": 2,
            "minor": 1,
            "capabilities": [
                "process.spawn", "process.list", "process.inspect",
                "process.stop", "logs.read"
            ],
        })
        .to_string(),
    )
    .unwrap();
    // The product refuses an endpoint that anyone could have rewritten.
    std::fs::set_permissions(
        state_dir.join("ready.json"),
        std::os::unix::fs::PermissionsExt::from_mode(0o600),
    )
    .unwrap();
    std::fs::write(
        state_dir.join("tasks.json"),
        serde_json::to_vec(&vec![serde_json::json!({
            "snapshot": {
                "id": task_id,
                "program": "make",
                "args": ["up"],
                "cwd": repo,
                "status": "Running",
                "exit_code": null,
                "log": "started\n",
                "duration_ms": 0,
                "pid": dead_pid,
                "owner_scope": "owner-session",
            },
            "lifetime": "Session",
            "writer_scope": "parent",
            "write_scope": "Unrestricted",
            "process_id": dead_pid,
            "started_at_ms": 0,
            "finished_at_ms": null,
            "log_end": 8,
            "log_prefix_len": 0,
        })])
        .unwrap(),
    )
    .unwrap();
    std::fs::write(state_dir.join("intents.json"), b"[]").unwrap();
    state_dir
}

fn exited_group_leader() -> u32 {
    use std::os::unix::process::CommandExt;
    let mut child = Command::new("sh")
        .arg("-c")
        .arg("exit 0")
        .process_group(0)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn group leader");
    let pid = child.id();
    child.wait().expect("reap group leader");
    pid
}

fn assert_converged(state_dir: &Path, task_id: &str) {
    let tasks: serde_json::Value =
        serde_json::from_slice(&std::fs::read(state_dir.join("tasks.json")).unwrap()).unwrap();
    let task = tasks
        .as_array()
        .unwrap()
        .iter()
        .find(|task| task["snapshot"]["id"] == task_id)
        .expect("the record is kept");
    assert_eq!(task["snapshot"]["status"], "Unknown");
    assert_eq!(task["loss"]["previous"], "Running");
    assert_eq!(
        task["loss"]["process"], "Gone",
        "the process group was really gone"
    );
    assert!(
        !state_dir.join("ready.json").exists(),
        "the dead endpoint is no longer trusted"
    );
}

fn assert_no_execution_host_error(stderr: &str) {
    assert!(
        !stderr.contains("execution host error")
            && !stderr.contains("Execution Host unavailable")
            && !stderr.contains("residue kept"),
        "the lost host must not block startup: {stderr}"
    );
}

/// The documented fallback must actually work when the daemon cannot start:
/// `--in-process` reconciles the same records and opens the TUI in this process.
#[tokio::test]
async fn in_process_fallback_starts_after_the_host_was_lost() {
    let env = test_env();
    let task_id = "host-lost-in-process";
    let state_dir = plant_lost_host(&env, task_id);

    let first = env.run(&["tui", "--in-process"], Duration::from_secs(60));
    let (succeeded, stderr) = first;
    assert!(
        !succeeded,
        "the TUI cannot take over a non-tty; startup must still have happened: {stderr}"
    );
    assert_no_execution_host_error(&stderr);
    assert!(
        stderr.contains(task_id),
        "the user is told which task lost its controlling host: {stderr}"
    );
    assert_eq!(env.sessions().await.len(), 1, "the TUI opened a session");
    assert_converged(&state_dir, task_id);
    let session_id = env.sessions().await[0].id.as_str().to_string();

    // A second start converges nothing new and replays nothing.
    let (_, stderr) = env.run(&["tui", "--in-process"], Duration::from_secs(60));
    assert_no_execution_host_error(&stderr);
    assert_eq!(
        env.sessions().await.len(),
        2,
        "a second session, not a replay"
    );
    assert_converged(&state_dir, task_id);

    // Resuming the first session must not create a session, replay the lost
    // command, or disturb the converged record.
    let (_, stderr) = env.run(
        &["tui", "--session", &session_id, "--in-process"],
        Duration::from_secs(60),
    );
    assert_no_execution_host_error(&stderr);
    assert_eq!(
        env.sessions().await.len(),
        2,
        "a resume opens the existing session"
    );
    assert_converged(&state_dir, task_id);
}

/// The daemon path: `leveler serve` must become ready instead of exiting on the
/// residue, and `leveler tui` must reuse it.
#[tokio::test]
async fn daemon_starts_and_serves_the_tui_after_the_host_was_lost() {
    let env = test_env();
    let task_id = "host-lost-daemon";
    let state_dir = plant_lost_host(&env, task_id);

    let ready = env._tmp.path().join("daemon-ready.json");
    let mut daemon = ManagedChild::spawn(
        env.command(&["serve", "--ready-json"])
            .arg(&ready)
            .stdout(Stdio::null())
            .stderr(Stdio::null()),
    )
    .expect("spawn leveler serve");
    let deadline = Instant::now() + Duration::from_secs(30);
    while !ready.is_file() {
        if let Some(status) = daemon.try_wait().expect("daemon status") {
            panic!("the daemon exited before readiness ({status}) on a lost host's residue");
        }
        assert!(
            Instant::now() < deadline,
            "the daemon never became ready on a lost host's residue"
        );
        std::thread::sleep(Duration::from_millis(25));
    }

    let (succeeded, stderr) = env.run(&["tui"], Duration::from_secs(60));
    assert!(
        !succeeded,
        "the TUI cannot take over a non-tty; the daemon must still have created its session: {stderr}"
    );
    assert_no_execution_host_error(&stderr);
    assert_eq!(env.sessions().await.len(), 1, "the daemon opened a session");
    assert_converged(&state_dir, task_id);

    let _ = Command::new("kill")
        .arg("-2")
        .arg(daemon.id().to_string())
        .status();
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline {
        if daemon.try_wait().expect("daemon status").is_some() {
            break;
        }
        std::thread::sleep(Duration::from_millis(25));
    }
}
