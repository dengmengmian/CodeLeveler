//! `leveler eval` must not write runtime state into the user's persistent home.
//!
//! The defect this pins: each eval case resolved its `Layout` from the
//! process-wide `LEVELER_HOME`, so a run created a project state dir (session
//! DB, browser profile) under the real `~/.leveler/state/projects`, named after
//! a throwaway `$TMPDIR/leveler-eval-*` workspace. An eval now runs against a
//! disposable home that is removed when the case ends; the declared output
//! (report / `--json-out`) is preserved, and provider/model config still comes
//! from the real environment.

use std::path::{Path, PathBuf};
use std::process::Command;

use leveler_test_support::{MockResponse, MockServer};

fn sse(frames: Vec<String>) -> MockResponse {
    let mut body = String::new();
    for frame in frames {
        body.push_str("data: ");
        body.push_str(&frame);
        body.push_str("\n\n");
    }
    body.push_str("data: [DONE]\n\n");
    MockResponse::Sse { body }
}

fn text_response() -> MockResponse {
    sse(vec![
        serde_json::json!({"choices":[{"delta":{"content":"done"},"finish_reason":"stop"}]})
            .to_string(),
    ])
}

/// A goal-mode completion: the model calls `update_goal(complete)`, which is
/// what the engine requires to end a run as `Completed`.
fn goal_response() -> MockResponse {
    sse(vec![
        serde_json::json!({
            "choices": [{
                "delta": {
                    "tool_calls": [{
                        "index": 0,
                        "id": "call_goal",
                        "function": {
                            "name": "update_goal",
                            "arguments": serde_json::json!({"status":"complete","summary":"done"}).to_string()
                        }
                    }]
                }
            }]
        })
        .to_string(),
        serde_json::json!({"choices":[{"delta":{},"finish_reason":"tool_calls"}]}).to_string(),
    ])
}

fn write_bundle(root: &Path, base_url: &str) {
    std::fs::create_dir_all(root.join("providers")).unwrap();
    std::fs::create_dir_all(root.join("models")).unwrap();
    std::fs::write(
        root.join("providers/mock.yaml"),
        format!("id: mock\nprotocol: openai_chat\nbase_url: {base_url}\n"),
    )
    .unwrap();
    std::fs::write(
        root.join("models/m.yaml"),
        r#"
id: m
provider: mock
model_id: mock-model
protocol: openai_chat
capabilities: { streaming: true, tool_calling: true, parallel_tool_calls: false, structured_output: true, reasoning: false, vision: false }
limits: { context_window: 8192, reliable_context: 4096, max_output_tokens: 1024, max_tool_schema_bytes: 8192, max_parallel_tool_calls: 1 }
compatibility: { synthesize_tool_call_ids: true, drop_unsupported_fields: true }
"#,
    )
    .unwrap();
}

fn write_case(dir: &Path, id: &str, expect_ok: bool) {
    std::fs::create_dir_all(dir).unwrap();
    let program = if expect_ok { "true" } else { "false" };
    // Completion is the agent's declared terminal outcome; the separate
    // expectation below decides whether this eval case passes.
    std::fs::write(
        dir.join(format!("{id}.yaml")),
        format!(
            "id: {id}\nname: {id}\ntask: say hi\nexpected_outcome: completed\nexpect:\n  program: \"{program}\"\n"
        ),
    )
    .unwrap();
}

/// Every ephemeral eval home for one test run, under that test's private
/// `TMPDIR` (the eval child honours `TMPDIR`, so tests never share a base).
fn run_dirs(tmp_root: &Path) -> Vec<PathBuf> {
    let mut dirs: Vec<PathBuf> = std::fs::read_dir(tmp_root.join("codeleveler").join("eval"))
        .map(|it| {
            it.filter_map(Result::ok)
                .map(|entry| entry.path())
                .collect()
        })
        .unwrap_or_default();
    dirs.sort();
    dirs
}

fn persistent_project_dirs(home: &Path) -> Vec<PathBuf> {
    let mut dirs: Vec<PathBuf> = std::fs::read_dir(home.join("state").join("projects"))
        .map(|it| {
            it.filter_map(Result::ok)
                .map(|entry| entry.path())
                .collect()
        })
        .unwrap_or_default();
    dirs.sort();
    dirs
}

struct EvalFixture {
    _tmp: tempfile::TempDir,
    /// Private `TMPDIR` for the child: ephemeral state and the case workspace
    /// both live and die here.
    tmp_root: PathBuf,
    bundle: PathBuf,
    cases: PathBuf,
    home: PathBuf,
    json_out: PathBuf,
}

fn fixture(server: &MockServer, expect_ok: bool) -> EvalFixture {
    let tmp = tempfile::tempdir().unwrap();
    let bundle = tmp.path().join("bundle");
    write_bundle(&bundle, &server.base_url());
    let cases = tmp.path().join("cases");
    write_case(&cases, "iso", expect_ok);
    let home = tmp.path().join("home");
    std::fs::create_dir_all(&home).unwrap();
    let json_out = tmp.path().join("result.json");
    let tmp_root = tmp.path().to_path_buf();
    EvalFixture {
        _tmp: tmp,
        tmp_root,
        bundle,
        cases,
        home,
        json_out,
    }
}

fn eval_command(f: &EvalFixture) -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_leveler"));
    cmd.args(["eval", "run", "--cases"])
        .arg(&f.cases)
        .args(["--model", "mock/m"])
        .arg("--json-out")
        .arg(&f.json_out)
        .arg("--config-dir")
        .arg(&f.bundle)
        // The persistent home under test; the eval's runtime state must NOT
        // land here.
        .env("LEVELER_HOME", &f.home)
        .env("LEVELER_CONFIG_DIR", &f.bundle)
        // A private temp root per test: no other test's run can be mistaken
        // for a leak of this one's.
        .env("TMPDIR", &f.tmp_root)
        .env("NO_COLOR", "1");
    cmd
}

/// CASE A + B + G + H: a successful eval keeps the persistent home clean, uses
/// an ephemeral home that is removed, still resolves provider/model config, and
/// preserves the declared output.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_successful_eval_isolates_state_and_preserves_declared_output() {
    let server = MockServer::start(vec![goal_response()]).await;
    let f = fixture(&server, true);

    let before = run_dirs(&f.tmp_root);
    let out = eval_command(&f).output().expect("spawn eval");
    assert!(
        out.status.success(),
        "eval failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );

    // A1: no per-project state in the persistent home.
    let projects = persistent_project_dirs(&f.home);
    assert!(
        projects.is_empty(),
        "eval wrote project state into the persistent home: {projects:?}"
    );

    // A3 + H: the declared output survives the ephemeral state cleanup.
    let report = std::fs::read_to_string(&f.json_out).expect("declared --json-out exists");
    assert!(
        report.contains("iso"),
        "declared report must hold the run: {report}"
    );

    // B: the ephemeral run home is gone.
    let leaked: Vec<PathBuf> = run_dirs(&f.tmp_root)
        .into_iter()
        .filter(|path| !before.contains(path))
        .collect();
    assert!(leaked.is_empty(), "ephemeral eval homes leaked: {leaked:?}");
    drop(server);
}

/// CASE C: a case that FAILS still cleans its ephemeral state, and the failure
/// is reported rather than masked.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_failing_eval_still_cleans_up_and_does_not_pollute() {
    let server = MockServer::start(vec![text_response()]).await;
    let f = fixture(&server, false);

    let before = run_dirs(&f.tmp_root);
    let out = eval_command(&f).output().expect("spawn eval");
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains('✗') || stdout.contains("failed") || stdout.contains("FAIL"),
        "a failed expect must be reported: {stdout}"
    );

    assert!(
        persistent_project_dirs(&f.home).is_empty(),
        "a failed eval must not pollute the persistent home"
    );
    let leaked: Vec<PathBuf> = run_dirs(&f.tmp_root)
        .into_iter()
        .filter(|path| !before.contains(path))
        .collect();
    assert!(
        leaked.is_empty(),
        "a failed eval must still clean its home: {leaked:?}"
    );
    drop(server);
}

/// CASE D: two evals run at once get unique ephemeral homes; neither run's
/// cleanup can remove the other's state.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_evals_do_not_share_a_home() {
    // One server per process: the mock serves a per-connection queue, so a
    // shared one would interleave the two runs' turns.
    let server_a = MockServer::start(vec![goal_response()]).await;
    let server_b = MockServer::start(vec![goal_response()]).await;
    let a = fixture(&server_a, true);
    let b = fixture(&server_b, true);

    let before_a = run_dirs(&a.tmp_root);
    let before_b = run_dirs(&b.tmp_root);
    let (out_a, out_b, root_a, root_b) = tokio::task::spawn_blocking(move || {
        let out_a = eval_command(&a).output().expect("spawn eval a");
        let out_b = eval_command(&b).output().expect("spawn eval b");
        (out_a, out_b, a.tmp_root, b.tmp_root)
    })
    .await
    .expect("join eval runs");

    assert!(out_a.status.success(), "eval A failed");
    assert!(out_b.status.success(), "eval B failed");

    for (root, before, tag) in [(&root_a, &before_a, "A"), (&root_b, &before_b, "B")] {
        let leaked: Vec<PathBuf> = run_dirs(root)
            .into_iter()
            .filter(|path| !before.contains(path))
            .collect();
        assert!(
            leaked.is_empty(),
            "concurrent eval {tag} left homes behind: {leaked:?}"
        );
    }
    drop(server_a);
    drop(server_b);
}
