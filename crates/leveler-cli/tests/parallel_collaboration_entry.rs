//! Entry closure for the parallel run path.
//!
//! `leveler run --parallel N --collaboration <axis>` must resolve the explicit
//! axis the CLI parsed onto the parallel parent row, and every candidate child
//! (a fresh `Application` per isolated worktree) must inherit that same axis.
//! The leak this locks: dispatch dropped the parsed axis on the `--parallel`
//! branch, so a parallel run silently reverted to the product default `goal`.
//!
//! Evidence is durable and mechanical: every session the run persisted (the
//! parallel parent plus one per candidate child, each in its own worktree state
//! namespace) carries the axis on its row, and each candidate actually ran a
//! turn.

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use leveler_storage::{Database, SessionRepository};
use leveler_test_support::git;
use leveler_test_support::{MockResponse, MockServer};

fn text_response(content: &str) -> MockResponse {
    let frame =
        serde_json::json!({"choices": [{"delta": {"content": content}, "finish_reason": "stop"}]})
            .to_string();
    MockResponse::sse(&[frame.as_str()])
}

/// One `update_goal { status: complete }` call plus its finish frame: a
/// candidate closes its goal in a single round.
fn goal_complete_response() -> MockResponse {
    let call = serde_json::json!({"choices": [{"delta": {"tool_calls": [{
        "index": 0,
        "id": "c-goal",
        "type": "function",
        "function": {
            "name": "update_goal",
            "arguments": serde_json::json!({"status": "complete", "summary": "done"}).to_string()
        }
    }]}}]})
    .to_string();
    let finish =
        serde_json::json!({"choices": [{"delta": {}, "finish_reason": "tool_calls"}]}).to_string();
    MockResponse::sse(&[call.as_str(), finish.as_str()])
}

/// A fully isolated environment: its own home (state), config bundle (mock
/// provider) and clean, committed repository. Parallel editing refuses a dirty
/// tree, so the seed commit is part of the fixture.
struct TestEnv {
    tmp: tempfile::TempDir,
    home: PathBuf,
    repo: PathBuf,
    configs: PathBuf,
}

fn test_env(base_url: &str) -> TestEnv {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("home");
    let repo = tmp.path().join("repo");
    let configs = tmp.path().join("configs");
    std::fs::create_dir_all(&home).unwrap();
    std::fs::create_dir_all(&repo).unwrap();
    std::fs::create_dir_all(configs.join("providers")).unwrap();
    std::fs::create_dir_all(configs.join("models")).unwrap();
    git::init_repo(&repo);
    std::fs::write(repo.join("seed.txt"), "seed\n").unwrap();
    git::run(&repo, &["add", "-A"]);
    git::run(&repo, &["commit", "-qm", "seed"]);
    std::fs::write(
        configs.join("providers/mock.yaml"),
        format!("id: mock\nprotocol: openai_chat\nbase_url: {base_url}\n"),
    )
    .unwrap();
    std::fs::write(
        configs.join("models/m.yaml"),
        r#"
id: m
provider: mock
model_id: mock-model
protocol: openai_chat
capabilities: { streaming: true, tool_calling: true, parallel_tool_calls: false, structured_output: true, reasoning: false, vision: false }
limits: { context_window: 131072, reliable_context: 65536, max_output_tokens: 1024, max_tool_schema_bytes: 8192, max_parallel_tool_calls: 1 }
compatibility: { synthesize_tool_call_ids: true, drop_unsupported_fields: true }
"#,
    )
    .unwrap();
    TestEnv {
        tmp,
        home,
        repo,
        configs,
    }
}

fn spawn_run(env: &TestEnv, args: &[&str], log: &Path) -> Child {
    Command::new(env!("CARGO_BIN_EXE_leveler"))
        .arg("--repo")
        .arg(&env.repo)
        .arg("run")
        .args(args)
        .env("LEVELER_HOME", &env.home)
        .env("LEVELER_CONFIG_DIR", &env.configs)
        .stdin(Stdio::null())
        .stdout(std::fs::File::create(log.with_extension("out")).unwrap())
        .stderr(std::fs::File::create(log).unwrap())
        .spawn()
        .expect("spawn leveler run --parallel")
}

fn read_log(log: &Path) -> String {
    format!(
        "--- stdout ---\n{}--- stderr ---\n{}",
        read_stdout(log),
        std::fs::read_to_string(log).unwrap_or_default()
    )
}

fn read_stdout(log: &Path) -> String {
    std::fs::read_to_string(log.with_extension("out")).unwrap_or_default()
}

async fn wait_for(mut child: Child, log: &Path, timeout: Duration) -> std::process::ExitStatus {
    let deadline = Instant::now() + timeout;
    loop {
        if let Some(status) = child.try_wait().expect("try_wait") {
            return status;
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            panic!(
                "leveler run --parallel never exited within {timeout:?}:\n{}",
                read_log(log)
            );
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// Every session this run persisted carries `goal == task`: the parallel parent
/// (workspace = the repository) and each candidate child (workspace = its
/// isolated worktree, a different state namespace). The axis contract is that
/// all of them resolved to the same collaboration mode.
async fn run_collaborations(env: &TestEnv, task: &str) -> Vec<String> {
    let projects = env.home.join("state/projects");
    let mut found = Vec::new();
    let entries = std::fs::read_dir(&projects)
        .unwrap_or_else(|e| panic!("no projects dir {}: {e}", projects.display()));
    for entry in entries.filter_map(Result::ok) {
        let db_path = entry.path().join("sessions.db");
        if !db_path.is_file() {
            continue;
        }
        let db = Database::connect(&db_path).await.unwrap();
        for session in SessionRepository::new(&db).list().await.unwrap() {
            if session.goal == task {
                found.push(session.collaboration);
            }
        }
    }
    found.sort();
    found
}

fn assert_candidate_count(bodies: &[String], log: &Path) {
    assert!(
        bodies.len() >= 2,
        "both candidates must run a turn (got {} model requests):\n{}",
        bodies.len(),
        read_log(log)
    );
}

/// A + D + E — an explicit `chat` axis is not overridden by the Application
/// default, and the candidate children inherit it.
#[tokio::test]
async fn explicit_chat_parallel_axis_reaches_parent_and_children() {
    let server = MockServer::start_one(text_response("done")).await;
    let env = test_env(&server.base_url());
    let log = env.tmp.path().join("run.log");
    let child = spawn_run(
        &env,
        &[
            "--parallel",
            "2",
            "--collaboration",
            "chat",
            "--auto-approve",
            "parallel chat axis",
        ],
        &log,
    );
    let status = wait_for(child, &log, Duration::from_secs(120)).await;
    // A chat candidate edits nothing, so the parallel run legitimately reports
    // "no integrable changes" (exit 1). The axis contract is the point here:
    // the run must reach the parallel result, not bail out early.
    assert!(
        status.code().is_some(),
        "leveler run did not exit normally:\n{}",
        read_log(&log)
    );
    assert!(
        read_stdout(&log).contains("Parallel result"),
        "the run never reached the parallel path:\n{}",
        read_log(&log)
    );

    let task = "parallel chat axis";
    assert_eq!(
        run_collaborations(&env, task).await,
        vec!["chat", "chat", "chat"],
        "an explicit `--collaboration chat` must reach the parent and both candidates"
    );
    let bodies = server.request_bodies().await;
    assert_candidate_count(&bodies, &log);
}

/// B — an explicit `goal` axis lands on the parent and its children.
#[tokio::test]
async fn explicit_goal_parallel_axis_reaches_parent_and_children() {
    let server = MockServer::start_one(goal_complete_response()).await;
    let env = test_env(&server.base_url());
    let log = env.tmp.path().join("run.log");
    let child = spawn_run(
        &env,
        &[
            "--parallel",
            "2",
            "--collaboration",
            "goal",
            "--auto-approve",
            "parallel goal axis",
        ],
        &log,
    );
    let status = wait_for(child, &log, Duration::from_secs(120)).await;
    assert!(status.success(), "leveler run failed:\n{}", read_log(&log));

    assert_eq!(
        run_collaborations(&env, "parallel goal axis").await,
        vec!["goal", "goal", "goal"]
    );
    let bodies = server.request_bodies().await;
    assert_candidate_count(&bodies, &log);
}

/// C — an omitted axis is the product default (goal) for the parallel path too.
#[tokio::test]
async fn omitted_parallel_axis_is_goal() {
    let server = MockServer::start_one(goal_complete_response()).await;
    let env = test_env(&server.base_url());
    let log = env.tmp.path().join("run.log");
    let child = spawn_run(
        &env,
        &["--parallel", "2", "--auto-approve", "parallel default axis"],
        &log,
    );
    let status = wait_for(child, &log, Duration::from_secs(120)).await;
    assert!(status.success(), "leveler run failed:\n{}", read_log(&log));

    assert_eq!(
        run_collaborations(&env, "parallel default axis").await,
        vec!["goal", "goal", "goal"],
        "an omitted axis must resolve to the product default"
    );
}
