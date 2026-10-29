//! Entry contract for the parallel run path.
//!
//! The parallel pipeline exists to integrate candidate edits: it runs one
//! agent per isolated worktree, commits whatever each candidate produced, and
//! merges the verified branches into the current branch. `chat` may end with a
//! textual answer and `plan` is a read-only overlay, so neither is guaranteed
//! to produce the integrable change this path selects on. `--parallel` (more
//! than one candidate) therefore requires `collaboration=goal`, and a
//! non-goal axis is refused before a parent session, a worktree, or a provider
//! request exists.
//!
//! The goal path still resolves the explicit CLI axis onto the parallel parent
//! row, and every candidate child (a fresh `Application` per isolated
//! worktree) inherits it. Evidence is durable and mechanical: every session the
//! run persisted carries the axis on its row.

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
        .expect("spawn leveler run")
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

/// Every session this run persisted carries the requested axis: the parallel
/// parent (workspace = the repository) and each candidate child (workspace = its
/// isolated worktree, a different state namespace). A refused run persists
/// nothing, so a missing state directory is an empty answer, never a panic.
async fn run_collaborations(env: &TestEnv, task: &str) -> Vec<String> {
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

/// How many goals each session that ran `task` opened.
///
/// This is the durable proof of which execution profile a run used: a goal
/// profile opens the goal, and the axis is read afterwards — a finished window
/// legitimately leaves `chat` behind it.
async fn run_goal_counts(env: &TestEnv, task: &str) -> Vec<usize> {
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
        let stores = leveler_storage::EngineStores::from_database(&db);
        for session in SessionRepository::new(&db).list().await.unwrap() {
            if session.goal != task {
                continue;
            }
            let opened = match stores
                .tasks
                .task_for_session(&leveler_core::SessionId::new(session.id.as_str()))
                .await
                .unwrap()
            {
                Some(task_id) => stores.goals.for_task(&task_id).await.unwrap().len(),
                None => 0,
            };
            found.push(opened);
        }
    }
    found.sort();
    found
}

/// How many git worktrees the repository has: the main checkout plus one per
/// candidate. A refused run leaves exactly the main checkout.
fn worktree_count(repo: &Path) -> usize {
    let output = Command::new("git")
        .args(["worktree", "list", "--porcelain"])
        .current_dir(repo)
        .output()
        .expect("git worktree list");
    assert!(output.status.success(), "git worktree list failed");
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter(|line| line.starts_with("worktree "))
        .count()
}

fn assert_candidate_count(bodies: &[String], log: &Path) {
    assert!(
        bodies.len() >= 2,
        "both candidates must run a turn (got {} model requests):\n{}",
        bodies.len(),
        read_log(log)
    );
}

/// A + B + F + G — `--parallel` requires `collaboration=goal`. `chat` and
/// `plan` are refused at the execution entry, before a parent session, a
/// candidate child, a worktree, or a provider request exists. The refusal is
/// mechanical, not a fallback to goal and not an integration-stage failure.
#[tokio::test]
async fn parallel_rejects_non_goal_collaboration_before_any_work() {
    for axis in ["chat", "plan"] {
        let server = MockServer::start_one(goal_complete_response()).await;
        let env = test_env(&server.base_url());
        let log = env.tmp.path().join("run.log");
        let task = format!("parallel {axis} rejected");
        let child = spawn_run(
            &env,
            &[
                "--parallel",
                "3",
                "--collaboration",
                axis,
                "--auto-approve",
                &task,
            ],
            &log,
        );
        let status = wait_for(child, &log, Duration::from_secs(120)).await;
        let rendered = read_log(&log);

        assert!(
            !status.success(),
            "--parallel {axis} must fail fast, not run the pipeline:\n{rendered}"
        );
        // Never entered the pipeline: no result banner, no candidate work.
        assert!(
            !read_stdout(&log).contains("Parallel result"),
            "the parallel pipeline must not start:\n{rendered}"
        );
        // The error names the contract without binding the whole sentence.
        for needle in ["parallel", "collaboration=goal", axis] {
            assert!(
                rendered.contains(needle),
                "error must mention `{needle}`:\n{rendered}"
            );
        }
        // Strong evidence: the refusal precedes every side effect.
        assert!(
            server.request_bodies().await.is_empty(),
            "a refused combination must not call the provider:\n{rendered}"
        );
        assert!(
            run_collaborations(&env, &task).await.is_empty(),
            "a refused combination must persist no session:\n{rendered}"
        );
        assert_eq!(
            worktree_count(&env.repo),
            1,
            "a refused combination must create no worktree:\n{rendered}"
        );
    }
}

/// C — an explicit `goal` axis still enters the parallel pipeline and lands on
/// the parent and its children.
#[tokio::test]
async fn explicit_goal_parallel_axis_reaches_parent_and_children() {
    let server = MockServer::start_one(goal_complete_response()).await;
    let env = test_env(&server.base_url());
    let log = env.tmp.path().join("run.log");
    let child = spawn_run(
        &env,
        &[
            "--parallel",
            "3",
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
        run_goal_counts(&env, "parallel goal axis").await,
        vec![0, 1, 1, 1],
        "the parent dispatches, and all three candidates must run the goal profile"
    );
    // Every candidate closed its own Goal route with its terminal. The parent
    // never opened a goal — it only dispatches — so it keeps the axis of a
    // goal-run session: a later message there declares a new goal instead of
    // silently continuing work nobody ran.
    assert_eq!(
        run_collaborations(&env, "parallel goal axis").await,
        vec!["chat", "chat", "chat", "goal"],
        "candidate windows end their Goal route; the goal-less dispatcher keeps its declaration axis"
    );
    let bodies = server.request_bodies().await;
    assert_candidate_count(&bodies, &log);
}

/// D — an omitted axis is the product default (goal) for the parallel path too.
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
        run_goal_counts(&env, "parallel default axis").await,
        vec![0, 1, 1],
        "an omitted axis must resolve to the product default (goal) for the candidates"
    );
}

/// E — `chat` without `--parallel` is not part of the parallel contract and
/// stays legal: the new refusal must not capture the single-agent path.
///
/// The single-agent terminal itself is owned by the collaboration dispatch and
/// covered end-to-end in `headless_collaboration_execution.rs`; here only the
/// boundary is asserted.
#[tokio::test]
async fn chat_without_parallel_is_not_refused_by_the_parallel_contract() {
    let server = MockServer::start_one(text_response("done")).await;
    let env = test_env(&server.base_url());
    let log = env.tmp.path().join("run.log");
    let child = spawn_run(
        &env,
        &[
            "--collaboration",
            "chat",
            "--auto-approve",
            "chat without parallel",
        ],
        &log,
    );
    let status = wait_for(child, &log, Duration::from_secs(120)).await;
    let rendered = read_log(&log);

    assert!(
        status.success(),
        "a single-agent chat run must succeed:\n{rendered}"
    );
    assert!(
        !rendered.contains("collaboration=goal"),
        "the parallel contract must not capture a single-agent run:\n{rendered}"
    );
    assert_eq!(
        run_collaborations(&env, "chat without parallel").await,
        vec!["chat"],
        "an explicit chat axis must reach the single-agent session"
    );
    assert!(
        !server.request_bodies().await.is_empty(),
        "a legal single-agent chat run must reach the provider:\n{rendered}"
    );
}

#[tokio::test]
async fn parallel_rejects_unsupported_shipping_before_any_work() {
    let server = MockServer::start_one(goal_complete_response()).await;
    let env = test_env(&server.base_url());
    let log = env.tmp.path().join("unsupported.log");
    let child = spawn_run(
        &env,
        &[
            "--parallel",
            "2",
            "--push",
            "--auto-approve",
            "unsupported shipping",
        ],
        &log,
    );
    let status = wait_for(child, &log, Duration::from_secs(30)).await;
    assert!(
        !status.success(),
        "unsupported shipping must fail: {}",
        read_log(&log)
    );
    assert!(read_log(&log).contains("shipping"), "{}", read_log(&log));
    assert_eq!(server.request_count(), 0);
    assert_eq!(worktree_count(&env.repo), 1);
}

#[tokio::test]
async fn parallel_preserves_network_denial_and_json_output() {
    let server = MockServer::start_one(goal_complete_response()).await;
    let env = test_env(&server.base_url());
    let log = env.tmp.path().join("policy.log");
    let task = "parallel policy";
    let child = spawn_run(
        &env,
        &[
            "--parallel",
            "2",
            "--deny-network",
            "--max-model-steps",
            "1",
            "--output",
            "jsonl",
            "--auto-approve",
            task,
        ],
        &log,
    );
    let status = wait_for(child, &log, Duration::from_secs(30)).await;
    assert!(status.success(), "{}", read_log(&log));
    for line in read_stdout(&log).lines() {
        serde_json::from_str::<serde_json::Value>(line).expect("stdout must be JSONL");
    }
    let mut sessions = 0;
    for entry in std::fs::read_dir(env.home.join("state/projects"))
        .unwrap()
        .flatten()
    {
        let path = entry.path().join("sessions.db");
        if !path.is_file() {
            continue;
        }
        let db = Database::connect(&path).await.unwrap();
        let repo = SessionRepository::new(&db);
        for row in repo
            .list()
            .await
            .unwrap()
            .into_iter()
            .filter(|row| row.goal == task)
        {
            let (_, sandbox, _, _) = repo
                .execution(&leveler_core::SessionId::new(row.id))
                .await
                .unwrap()
                .unwrap();
            assert!(
                sandbox,
                "every parallel session must persist --deny-network"
            );
            sessions += 1;
        }
    }
    assert_eq!(sessions, 3);
}

#[cfg(unix)]
#[tokio::test]
async fn parallel_reports_cleanup_warnings_without_losing_integration() {
    use std::os::unix::fs::PermissionsExt;
    let real_git = std::env::split_paths(&std::env::var_os("PATH").unwrap())
        .map(|dir| dir.join("git"))
        .find(|path| path.is_file())
        .unwrap();
    for format in ["jsonl", "text"] {
        let server = MockServer::start_one(goal_complete_response()).await;
        let env = test_env(&server.base_url());
        let bin = env.tmp.path().join("bin");
        std::fs::create_dir(&bin).unwrap();
        let wrapper = bin.join("git");
        // Lock real worktrees immediately after creation. The implementation
        // must report Git's real refusal, without unlocking a user's resource.
        let quoted_git = format!("'{}'", real_git.to_str().unwrap().replace('\'', "'\\''"));
        std::fs::write(
            &wrapper,
            format!(
                r#"#!/bin/sh
if [ "$1" = worktree ] && [ "$2" = add ]; then
    {quoted_git} "$@" || exit $?
    exec {quoted_git} worktree lock "$5"
fi
exec {quoted_git} "$@"
"#
            ),
        )
        .unwrap();
        std::fs::set_permissions(&wrapper, std::fs::Permissions::from_mode(0o755)).unwrap();
        let paths = std::iter::once(bin)
            .chain(std::env::split_paths(&std::env::var_os("PATH").unwrap()))
            .collect::<Vec<_>>();
        let log = env.tmp.path().join("run.log");
        let child = Command::new(env!("CARGO_BIN_EXE_leveler"))
            .arg("--repo")
            .arg(&env.repo)
            .arg("run")
            .args([
                "--parallel",
                "2",
                "--auto-approve",
                "--output",
                format,
                "cleanup warning probe",
            ])
            .env("PATH", std::env::join_paths(paths).unwrap())
            .env("LEVELER_HOME", &env.home)
            .env("LEVELER_CONFIG_DIR", &env.configs)
            .stdin(Stdio::null())
            .stdout(std::fs::File::create(log.with_extension("out")).unwrap())
            .stderr(std::fs::File::create(&log).unwrap())
            .spawn()
            .unwrap();
        let status = wait_for(child, &log, Duration::from_secs(60)).await;
        let stdout = read_stdout(&log);
        let listing = Command::new(&real_git)
            .args(["worktree", "list", "--porcelain"])
            .current_dir(&env.repo)
            .output()
            .unwrap();
        assert!(listing.status.success());
        let listing = String::from_utf8(listing.stdout).unwrap();
        let retained = listing
            .lines()
            .filter_map(|line| line.strip_prefix("worktree "))
            .map(PathBuf::from)
            .filter(|path| path != &env.repo.canonicalize().unwrap())
            .collect::<Vec<_>>();
        // Remove only this fixture's retained worktrees before assertions.
        for path in &retained {
            git::run(&env.repo, &["worktree", "unlock", path.to_str().unwrap()]);
            git::run(
                &env.repo,
                &["worktree", "remove", "--force", path.to_str().unwrap()],
            );
        }
        assert!(status.success(), "{}", read_log(&log));
        assert_eq!(retained.len(), 2, "the locks must prevent cleanup");
        if format == "jsonl" {
            let outcome = stdout
                .lines()
                .map(|line| serde_json::from_str::<serde_json::Value>(line).unwrap())
                .find(|row| row["type"] == "parallel_completed")
                .unwrap();
            assert_eq!(outcome["integrated"].as_array().unwrap().len(), 2);
            let warnings = outcome["warnings"]
                .as_array()
                .expect("cleanup warnings must reach JSONL clients");
            assert_eq!(warnings.len(), 2);
            for path in &retained {
                assert!(warnings.iter().any(|warning| {
                    warning.as_str().unwrap().contains(
                        std::env::temp_dir()
                            .join(path.file_name().unwrap())
                            .to_str()
                            .unwrap(),
                    )
                }));
            }
        } else {
            for path in &retained {
                assert!(
                    stdout.contains(
                        std::env::temp_dir()
                            .join(path.file_name().unwrap())
                            .to_str()
                            .unwrap()
                    ),
                    "cleanup warning missing: {stdout}"
                );
            }
        }
        let mut terminals = Vec::new();
        for entry in std::fs::read_dir(env.home.join("state/projects")).unwrap() {
            let path = entry.unwrap().path().join("sessions.db");
            if !path.exists() {
                continue;
            }
            let db = Database::connect(&path).await.unwrap();
            for session in SessionRepository::new(&db).list().await.unwrap() {
                let events = leveler_storage::EventRepository::new(&db)
                    .load(&leveler_core::SessionId::new(session.id))
                    .await
                    .unwrap();
                for event in events {
                    if let leveler_engine::EngineEvent::TaskFinished { warnings, .. } =
                        leveler_engine::EngineEvent::from_payload(&event.payload).unwrap()
                        && warnings.len() == 2
                    {
                        terminals.push(warnings);
                    }
                }
            }
        }
        assert_eq!(
            terminals.len(),
            1,
            "cleanup warnings must survive in the parent's terminal fact"
        );
    }
}
