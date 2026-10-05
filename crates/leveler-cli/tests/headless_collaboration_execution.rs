//! Headless `leveler run` executes the collaboration axis it was given.
//!
//! `sessions.collaboration` is the durable fact, but it must also decide the
//! execution profile: before this, `run_in_session` called the goal engine
//! unconditionally, so `leveler run --collaboration chat` persisted `chat` and
//! still ran a Goal — quiet continuations, then `Stalled` (exit 3). These tests
//! drive the real binary against a mock provider and observe the terminal, the
//! exit code, the number of model requests and what the request carried.

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
        .expect("spawn leveler run")
}

fn read_stdout(log: &Path) -> String {
    std::fs::read_to_string(log.with_extension("out")).unwrap_or_default()
}

fn read_log(log: &Path) -> String {
    format!(
        "--- stdout ---\n{}--- stderr ---\n{}",
        read_stdout(log),
        std::fs::read_to_string(log).unwrap_or_default()
    )
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
                "leveler run never exited within {timeout:?}:\n{}",
                read_log(log)
            );
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// The axis each persisted session for `task` carries.
async fn session_axes(env: &TestEnv, task: &str) -> Vec<String> {
    let projects = env.home.join("state/projects");
    let Ok(entries) = std::fs::read_dir(&projects) else {
        return Vec::new();
    };
    let mut found = Vec::new();
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

/// A — `--collaboration chat` runs the Chat profile: the assistant final is the
/// terminal, the run exits success, and no goal continuation ever starts (one
/// model request, and the request carries no `update_goal`).
#[tokio::test]
async fn headless_chat_ends_on_the_answer() {
    let server = MockServer::start_one(text_response("hello")).await;
    let env = test_env(&server.base_url());
    let log = env.tmp.path().join("run.log");
    let task = "chat smoke";
    let child = spawn_run(
        &env,
        &["--collaboration", "chat", "--auto-approve", task],
        &log,
    );
    let status = wait_for(child, &log, Duration::from_secs(120)).await;
    let rendered = read_log(&log);

    assert!(status.success(), "chat must exit success:\n{rendered}");
    assert!(
        read_stdout(&log).contains("Answered"),
        "the answer must be the reported terminal:\n{rendered}"
    );
    for goal_only in ["Stalled", "nudge", "closeout_goal_unresolved"] {
        assert!(
            !rendered.contains(goal_only),
            "a chat run must not enter the goal continuation (`{goal_only}`):\n{rendered}"
        );
    }
    assert_eq!(
        server.request_count(),
        1,
        "a chat turn is not continued:\n{rendered}"
    );
    let bodies = server.request_bodies().await;
    assert!(
        !bodies[0].contains("\"update_goal\""),
        "a chat request must not carry the goal executor:\n{rendered}"
    );
    assert_eq!(session_axes(&env, task).await, vec!["chat"]);
}

/// B — `--collaboration goal` keeps the Goal terminal: an explicit
/// `update_goal(complete)` is success, and a quiet final is continued instead
/// of being accepted as completion.
#[tokio::test]
async fn headless_goal_keeps_the_goal_terminal_contract() {
    let server = MockServer::start_one(goal_complete_response()).await;
    let env = test_env(&server.base_url());
    let log = env.tmp.path().join("run.log");
    let child = spawn_run(
        &env,
        &["--collaboration", "goal", "--auto-approve", "goal complete"],
        &log,
    );
    let status = wait_for(child, &log, Duration::from_secs(120)).await;
    assert!(status.success(), "leveler run failed:\n{}", read_log(&log));
    assert!(
        read_stdout(&log).contains("Completed"),
        "update_goal(complete) is the goal terminal:\n{}",
        read_log(&log)
    );
    assert_eq!(session_axes(&env, "goal complete").await, vec!["goal"]);

    // The same axis with only a quiet answer stays unresolved.
    let server = MockServer::start_one(text_response("looks done to me")).await;
    let env = test_env(&server.base_url());
    let log = env.tmp.path().join("run.log");
    let child = spawn_run(
        &env,
        &["--collaboration", "goal", "--auto-approve", "goal quiet"],
        &log,
    );
    let status = wait_for(child, &log, Duration::from_secs(120)).await;
    let rendered = read_log(&log);
    assert!(
        !status.success(),
        "a quiet goal answer is not a completion:\n{rendered}"
    );
    assert!(
        read_stdout(&log).contains("went quiet"),
        "the goal must be continued, then report the honest stall:\n{rendered}"
    );
    assert!(
        server.request_count() > 1,
        "a quiet Goal must be continued (got {} request(s)):\n{rendered}",
        server.request_count()
    );
}

/// C — `--collaboration plan` keeps the Plan contract: the answer terminal,
/// no completion authority in the request, and the axis stays `plan`.
#[tokio::test]
async fn headless_plan_keeps_the_plan_contract() {
    let server = MockServer::start_one(text_response("plan: do X")).await;
    let env = test_env(&server.base_url());
    let log = env.tmp.path().join("run.log");
    let child = spawn_run(
        &env,
        &["--collaboration", "plan", "--auto-approve", "plan smoke"],
        &log,
    );
    let status = wait_for(child, &log, Duration::from_secs(120)).await;
    let rendered = read_log(&log);

    assert!(status.success(), "plan must exit success:\n{rendered}");
    assert_eq!(
        server.request_count(),
        1,
        "a plan turn is not continued by the goal lifecycle:\n{rendered}"
    );
    let bodies = server.request_bodies().await;
    assert!(
        !bodies[0].contains("\"update_goal\""),
        "plan has no completion authority:\n{rendered}"
    );
    assert_eq!(session_axes(&env, "plan smoke").await, vec!["plan"]);
}

/// D — an omitted `--collaboration` is the product default Goal.
#[tokio::test]
async fn omitted_collaboration_is_goal() {
    let server = MockServer::start_one(goal_complete_response()).await;
    let env = test_env(&server.base_url());
    let log = env.tmp.path().join("run.log");
    let child = spawn_run(&env, &["--auto-approve", "goal default"], &log);
    let status = wait_for(child, &log, Duration::from_secs(120)).await;
    assert!(status.success(), "leveler run failed:\n{}", read_log(&log));
    assert_eq!(session_axes(&env, "goal default").await, vec!["goal"]);
}
