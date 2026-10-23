//! Agent-experience extraction after a completed task.
//!
//! These tests drive the REAL runtime: a scripted provider runs tool calls
//! through the real execution host, the turn reaches its durable terminal, and
//! the runtime-owned consolidator extracts and stores the lesson. The provider
//! response for the extraction call is scripted, but the evidence pack and the
//! verification decision are the production ones.

use std::sync::Arc;
use std::time::Duration;

use leveler_app::Application;
use leveler_execution::PermissionProfile;
use leveler_memory::MemoryView;
use leveler_model::ModelRef;
use leveler_project::Layout;
use leveler_test_support::{MockResponse, MockServer};

fn isolate_global_config() {
    use std::sync::OnceLock;
    static EMPTY_HOME: OnceLock<tempfile::TempDir> = OnceLock::new();
    let dir = EMPTY_HOME.get_or_init(|| tempfile::tempdir().unwrap());
    unsafe {
        std::env::set_var("LEVELER_HOME", dir.path());
    }
}

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

/// A non-streaming completion. Both extractors use `generate`, so their
/// responses must be JSON, not an SSE stream.
fn json_text(content: &str) -> MockResponse {
    MockResponse::json_ok(
        &serde_json::json!({
            "id": "chatcmpl-test",
            "object": "chat.completion",
            "choices": [{
                "index": 0,
                "message": {"role": "assistant", "content": content},
                "finish_reason": "stop"
            }],
            "usage": {"prompt_tokens": 20, "completion_tokens": 20, "total_tokens": 40}
        })
        .to_string(),
    )
}

fn text(content: &str) -> MockResponse {
    sse(vec![
        serde_json::json!({"choices": [{"delta": {"content": content}, "finish_reason": "stop"}]})
            .to_string(),
    ])
}

fn tool_call(id: &str, name: &str, arguments: serde_json::Value) -> MockResponse {
    sse(vec![
        serde_json::json!({"choices": [{"delta": {"tool_calls": [{
            "index": 0,
            "id": id,
            "type": "function",
            "function": {"name": name, "arguments": arguments.to_string()}
        }]}}]})
        .to_string(),
        serde_json::json!({"choices": [{"delta": {}, "finish_reason": "tool_calls"}]}).to_string(),
    ])
}

fn check_marker() -> serde_json::Value {
    serde_json::json!({"program": "sh", "args": ["-c", "test -f marker.txt"]})
}

fn complete_goal() -> MockResponse {
    tool_call(
        "c-goal",
        "update_goal",
        serde_json::json!({
            "status": "complete",
            "summary": "workspace check passes",
            "verification": {
                "status": "passed",
                "detail": "sh -c test -f marker.txt"
            }
        }),
    )
}

fn failing_cargo_test() -> serde_json::Value {
    serde_json::json!({"program": "cargo", "args": ["test"], "reason": "reproduce the failure"})
}

fn write_marker() -> serde_json::Value {
    serde_json::json!({"path": "marker.txt", "content": "ok"})
}

fn check_marker_two() -> serde_json::Value {
    serde_json::json!({"program": "sh", "args": ["-c", "test -f marker2.txt"]})
}

fn write_marker_two() -> serde_json::Value {
    serde_json::json!({"path": "marker2.txt", "content": "ok"})
}

/// A complete lesson for the marker task. Evidence ids are derived from the
/// scripted call ids and model steps, exactly as the runtime names them.
fn verified_lesson() -> MockResponse {
    verified_lesson_ids("failure-1-c-fail", "resolution-3-c-pass")
}

fn verified_lesson_ids(failure: &str, resolution: &str) -> MockResponse {
    let body = format!(
        r#"{{"experiences":[{{"category":"root_cause","subject":"experience.workspace.marker","title":"检查失败源于 marker.txt 缺失","body":"工作区检查依赖 marker.txt；文件缺失时检查先失败，创建该文件后同一个检查通过。这个结论只说明文件存在改变了行为，具体调用链仍需按当前代码确认。","applies_when":"运行 workspace 检查且工作区可能缺少 marker.txt","invalid_if":"检查脚本不再依赖 marker.txt","evidence_ids":["{failure}","{resolution}"],"confidence":0.9}}]}}"#
    );
    json_text(&body)
}

fn unverified_lesson() -> MockResponse {
    json_text(
        r#"{"experiences":[{"category":"failure_pattern","subject":"experience.workspace.marker","title":"cargo test 在当前工作区反复失败","body":"当前工作区连续两次运行 cargo test 都失败，可能是缺少测试环境；证据只能说明失败重复发生，不能证明根因。","applies_when":"在缺少 Cargo.toml 的临时工作区运行 cargo test","invalid_if":"工作区补上 Cargo.toml 或测试环境","evidence_ids":["failure-1-c-fail"],"confidence":0.8}]}"#,
    )
}

fn updated_lesson() -> MockResponse {
    json_text(
        r#"{"experiences":[{"category":"root_cause","subject":"experience.workspace.marker","title":"检查失败源于 marker.txt 缺失","body":"新的证据指向真实原因是检查脚本把 marker.txt 当成必需输入；文件缺失时失败，创建后通过。旧解释应被替换。","applies_when":"运行 workspace 检查且工作区可能缺少 marker.txt","invalid_if":"检查脚本不再依赖 marker.txt","evidence_ids":["failure-1-c-fail","resolution-3-c-pass"],"confidence":0.9}]}"#,
    )
}

fn engine_script(required_profile: &str) -> String {
    format!(
        "#!/bin/sh\nset -eu\nprofile=$(sed -n 's/^profile=//p' engine.config | tail -n 1)\nif [ \"$profile\" != \"{required_profile}\" ]; then\n  echo \"error: engine profile rejected\" >&2\n  exit 1\nfi\necho \"engine: build ok (profile=$profile)\"\n"
    )
}

fn check_script() -> serde_json::Value {
    serde_json::json!({"program": "sh", "args": ["./check.sh"], "reason": "run the project check"})
}

/// A lesson earned while the installed engine still accepted `legacy`.
fn legacy_profile_lesson() -> MockResponse {
    json_text(
        r#"{"experiences":[{"category":"fix","subject":"experience.engine.legacy_profile","title":"engine profile 拒绝时改用 legacy","body":"当 ./check.sh 以 `error: engine profile rejected` 失败时，修改 engine.config 把 profile 设为 legacy；重新运行后回执报告 profile=legacy。","applies_when":"项目检查脚本报告 engine profile 被拒绝且只能修改项目文件","invalid_if":"安装的引擎不再校验配置里的 profile","evidence_ids":["failure-1-c-fail","resolution-3-c-pass"],"confidence":0.9}]}"#,
    )
}

/// The same failure signature after the installed engine changed: the
/// environment now accepts `current`, not `legacy`.
fn current_profile_lesson() -> MockResponse {
    json_text(
        r#"{"experiences":[{"category":"fix","subject":"experience.engine.current_profile","title":"engine profile 拒绝时改用 current","body":"当 ./check.sh 以 `error: engine profile rejected` 失败时，修改 engine.config 把 profile 设为 current；重新运行后回执报告 profile=current。旧环境里的 legacy 已不再适用。","applies_when":"项目检查脚本报告 engine profile 被拒绝且只能修改项目文件","invalid_if":"安装的引擎不再校验配置里的 profile","evidence_ids":["failure-1-c-fail","resolution-3-c-pass"],"confidence":0.9}]}"#,
    )
}

/// A file inside the workspace the run actually executes in.
fn workspace_file(tmp: &tempfile::TempDir, name: &str) -> std::path::PathBuf {
    tmp.path().join("workspace").join(name)
}

async fn harness(
    responses: Vec<MockResponse>,
) -> (tempfile::TempDir, MockServer, Arc<Application>) {
    isolate_global_config();
    let server = MockServer::start(responses).await;
    let tmp = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(tmp.path().join("configs/providers")).unwrap();
    std::fs::create_dir_all(tmp.path().join("configs/models")).unwrap();
    std::fs::write(
        tmp.path().join("configs/providers/mock.yaml"),
        format!(
            "id: mock\nprotocol: openai_chat\nbase_url: {}\n",
            server.base_url()
        ),
    )
    .unwrap();
    std::fs::write(
        tmp.path().join("configs/models/m.yaml"),
        r#"
id: m
provider: mock
model_id: mock-model
protocol: openai_chat
capabilities: { streaming: true, tool_calling: true, parallel_tool_calls: false, structured_output: true, reasoning: false, vision: false }
limits: { context_window: 131072, reliable_context: 65536, max_output_tokens: 1024, max_tool_schema_bytes: 65536, max_parallel_tool_calls: 1 }
compatibility: { synthesize_tool_call_ids: true, drop_unsupported_fields: true }
"#,
    )
    .unwrap();
    // The workspace is separate from the private state root: Memory 2.0 keeps
    // its journal and index outside the repository by contract.
    std::fs::create_dir_all(tmp.path().join("workspace/.leveler")).unwrap();
    std::fs::write(
        tmp.path().join("workspace/.leveler/config.yaml"),
        "memory:\n  enabled: true\n  experience_enabled: true\n",
    )
    .unwrap();
    let layout = Layout::from_parts(
        tmp.path().join("workspace"),
        tmp.path().join("configs"),
        tmp.path().join("state"),
    );
    let app = Arc::new(
        Application::assemble(layout)
            .unwrap()
            .with_collaboration(leveler_agent::CollaborationMode::Chat),
    );
    (tmp, server, app)
}

async fn run_chat(app: &Application, task: &str) -> leveler_core::SessionId {
    let model = ModelRef::new("mock", "m");
    let session = app.create_session(&model, task).await.unwrap();
    app.run_in_session(
        &session,
        &model,
        PermissionProfile::Assisted,
        task,
        Arc::new(leveler_execution::AutoApprove),
        false,
        &mut |_| {},
        tokio_util::sync::CancellationToken::new(),
    )
    .await
    .unwrap();
    session
}

/// Current records that carry an experience record.
fn experience_views(app: &Application) -> Vec<MemoryView> {
    app.memory_service()
        .listing_for_user(
            leveler_memory::ServiceScope::Project,
            leveler_memory::MemoryLanes::ALL,
        )
        .unwrap()
        .into_iter()
        .filter(|view| view.experience.is_some())
        .collect()
}

async fn wait_for_experience(
    app: &Application,
    predicate: impl Fn(&[MemoryView]) -> bool,
) -> Vec<MemoryView> {
    let waited = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let entries = experience_views(app);
            if predicate(&entries) {
                return entries;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await;
    match waited {
        Ok(entries) => entries,
        Err(_) => {
            let last = match app.open_database().await {
                Ok(db) => leveler_storage::ExperienceInboxRepository::new(&db)
                    .last_processed()
                    .await
                    .ok()
                    .flatten(),
                Err(_) => None,
            };
            panic!(
                "experience extraction must settle: views={:?} last={last:?}",
                experience_views(app)
            )
        }
    }
}

/// Pending experience proposals only a person can approve.
fn pending_experiences(app: &Application) -> Vec<MemoryView> {
    app.memory_service()
        .pending(
            &leveler_memory::v2::Actor::User {
                invocation: leveler_memory::v2::UserInvocationId::mint("experience-review"),
            },
            leveler_memory::ServiceScope::Project,
        )
        .unwrap()
        .into_iter()
        .filter(|view| view.experience.is_some())
        .collect()
}

/// Accept one pending experience as the user would from the TUI.
fn accept_experience(app: &Application, id: &str) {
    app.memory_service()
        .review(
            &leveler_memory::v2::Actor::User {
                invocation: leveler_memory::v2::UserInvocationId::mint("experience-accept"),
            },
            leveler_memory::ServiceScope::Project,
            id,
            leveler_memory::ReviewRequest::Accept,
        )
        .unwrap();
}

async fn wait_for_experience_inbox_settled(app: &Application) {
    let db = app.open_database().await.unwrap();
    let repo = leveler_storage::ExperienceInboxRepository::new(&db);
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let counts = repo.counts().await.unwrap();
            if counts.processed >= 1
                && counts.pending == 0
                && counts.processing == 0
                && counts.failed_retryable == 0
            {
                return;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .expect("experience inbox must settle within the test deadline");
}

#[tokio::test]
async fn a_completed_task_with_a_verified_resolution_stores_a_verified_experience() {
    let (tmp, server, app) = harness(vec![
        tool_call("c-fail", "run_command", check_marker()),
        tool_call("c-fix", "write_file", write_marker()),
        tool_call("c-pass", "run_command", check_marker()),
        text("done"),
        verified_lesson(),
    ])
    .await;
    let session = run_chat(&app, "让 workspace 检查通过").await;
    let entries = wait_for_experience(&app, |entries| entries.len() == 1).await;
    let entry = &entries[0];
    assert!(entry.is_experience(), "the record is a lesson: {entry:?}");
    let record = entry.experience.as_ref().expect("structured record");
    assert_eq!(record.standing, "verified");
    assert!(
        record.evidence.iter().any(|item| item.0 == "failure"),
        "{record:?}"
    );
    assert!(
        record.evidence.iter().any(|item| item.0 == "resolution"),
        "{record:?}"
    );
    assert!(
        record.evidence.iter().any(|item| item.0 == "mutation"),
        "the verified lesson records the change that made it pass: {record:?}"
    );
    wait_for_experience_inbox_settled(&app).await;

    let db = app.open_database().await.unwrap();
    let calls = leveler_storage::ModelRequestRepository::new(&db)
        .load_for_session(&session)
        .await
        .unwrap();
    assert!(
        calls
            .iter()
            .any(|row| row.kind == leveler_storage::ModelCallKind::ExperienceExtraction),
        "the experience call must be recorded in its own lane"
    );

    let bodies = server.request_bodies().await;
    let extraction = bodies
        .iter()
        .find(|body| body.contains("extract durable, evidence-bound lessons"))
        .expect("the experience extractor must have been called");
    assert!(
        !extraction.contains("done"),
        "assistant prose is not evidence: {extraction}"
    );
}

/// Both execution lifecycles admit experience work. Chat is covered above;
/// this is the same lesson from an explicit Goal that closed through
/// `update_goal(complete)` and its mechanical verification gate.
#[tokio::test]
async fn a_goal_turn_also_extracts_an_experience() {
    let (tmp, _server, app) = harness(vec![
        tool_call("c-fail", "run_command", check_marker()),
        tool_call("c-fix", "write_file", write_marker()),
        tool_call("c-pass", "run_command", check_marker()),
        complete_goal(),
        text("done"),
        verified_lesson(),
    ])
    .await;
    let model = ModelRef::new("mock", "m");
    let session = app
        .create_session(&model, "让 workspace 检查通过")
        .await
        .unwrap();
    app.run_in_session(
        &session,
        &model,
        PermissionProfile::Assisted,
        "让 workspace 检查通过",
        Arc::new(leveler_execution::AutoApprove),
        false,
        &mut |_| {},
        tokio_util::sync::CancellationToken::new(),
    )
    .await
    .unwrap();
    let entries = wait_for_experience(&app, |entries| entries.len() == 1).await;
    assert_eq!(entries[0].experience.as_ref().unwrap().standing, "verified");
    wait_for_experience_inbox_settled(&app).await;
}

#[tokio::test]
async fn an_ordinary_task_does_not_pay_for_an_experience_extraction_call() {
    let (tmp, _server, app) = harness(vec![
        tool_call(
            "c-ok",
            "run_command",
            serde_json::json!({"program": "sh", "args": ["-c", "true"]}),
        ),
        text("done"),
    ])
    .await;
    let session = run_chat(&app, "确认环境").await;
    let _ = session;
    wait_for_experience_inbox_settled(&app).await;
    assert!(
        app.memory_service()
            .listing_for_user(
                leveler_memory::ServiceScope::Project,
                leveler_memory::MemoryLanes::ALL
            )
            .unwrap()
            .is_empty()
    );

    let db = app.open_database().await.unwrap();
    let processed = leveler_storage::ExperienceInboxRepository::new(&db)
        .last_processed()
        .await
        .unwrap()
        .expect("the row settled");
    let result = processed.result_json.unwrap();
    assert!(result.contains("no_experience_signal"), "{result}");
    let requests = leveler_storage::ModelRequestRepository::new(&db)
        .load_for_session(&session)
        .await
        .unwrap();
    assert!(
        !requests
            .iter()
            .any(|row| row.kind == leveler_storage::ModelCallKind::ExperienceExtraction),
        "a task without failure evidence must not call the extractor"
    );
}

/// A command that fails, then exits 0 with no output after the task changes
/// the workspace, is a structured silent resolution. The script's name is not
/// read; the identity is the exact invocation.
#[tokio::test]
async fn a_silent_verification_command_can_produce_a_verified_constraint() {
    let (tmp, _server, app) = harness(vec![
        tool_call(
            "c-fail",
            "run_command",
            serde_json::json!({"program": "sh", "args": ["verify.sh"], "reason": "run the check before the script exists"}),
        ),
        tool_call(
            "c-write",
            "write_file",
            serde_json::json!({"path": "verify.sh", "content": "#!/bin/sh\nexit 0\n"}),
        ),
        tool_call(
            "c-verify",
            "run_command",
            serde_json::json!({"program": "sh", "args": ["verify.sh"]}),
        ),
        text("done"),
        json_text(
            r#"{"experiences":[{"category":"environment_constraint","subject":"experience.workspace.silent_verifier","title":"同一个检查命令先失败后静默退出 0","body":"同一个 `sh verify.sh` 调用在脚本缺失时失败；创建脚本后再次运行没有任何输出就退出 0。这只能证明该命令现在退出成功，不能证明脚本检查了工作区。","applies_when":"运行同一个 `sh verify.sh` 调用且脚本存在","invalid_if":"verify.sh 输出检查结果或命令不再静默退出","evidence_ids":["observation-3-c-verify"],"confidence":0.8}]}"#,
        ),
    ])
    .await;
    run_chat(&app, "确认 verify.sh 真的检查了工作区").await;
    let entries = wait_for_experience(&app, |entries| entries.len() == 1).await;
    let record = entries[0].experience.as_ref().expect("record");
    // A silent exit 0 is an observation, not a resolution: Memory 2.0 derives
    // the standing from the evidence the record carries, so this lesson can
    // never read as a verified fix.
    assert_eq!(record.standing, "observed");
    assert!(
        record.evidence.iter().any(|item| item.0 == "observation"),
        "{record:?}"
    );
    assert!(
        !record.evidence.iter().any(|item| item.0 == "mutation"),
        "a constraint has no passing check to attribute a mutation to: {record:?}"
    );
    wait_for_experience_inbox_settled(&app).await;
}

/// A lesson built from failure evidence only is an observation: Memory 2.0
/// derives the standing from what the record carries, so it can be current and
/// still never read as a verified fix.
#[tokio::test]
async fn an_observation_only_lesson_is_current_as_observed_not_verified() {
    let (tmp, _server, app) = harness(vec![
        tool_call("c-fail", "run_command", failing_cargo_test()),
        text("the workspace has no Cargo.toml"),
        unverified_lesson(),
    ])
    .await;
    run_chat(&app, "诊断 cargo test 为什么失败").await;
    wait_for_experience_inbox_settled(&app).await;
    let current = app
        .memory_service()
        .listing_for_user(
            leveler_memory::ServiceScope::Project,
            leveler_memory::MemoryLanes::ALL,
        )
        .unwrap();
    let lesson = current
        .iter()
        .find(|entry| entry.experience.is_some())
        .expect("the observation is a lesson");
    assert_eq!(lesson.experience.as_ref().unwrap().standing, "observed");
    assert!(
        current.iter().all(|entry| entry
            .experience
            .as_ref()
            .map(|record| record.standing.as_str())
            != Some("verified")),
        "nothing may read as a verified fix: {current:?}"
    );
}

#[tokio::test]
async fn new_evidence_updates_the_previous_experience_instead_of_duplicating_it() {
    let (tmp, _server, app) = harness(vec![
        tool_call("c-fail", "run_command", check_marker()),
        tool_call("c-fix", "write_file", write_marker()),
        tool_call("c-pass", "run_command", check_marker()),
        text("done"),
        verified_lesson(),
        tool_call("c-fail", "run_command", check_marker_two()),
        tool_call("c-fix", "write_file", write_marker_two()),
        tool_call("c-pass", "run_command", check_marker_two()),
        text("done"),
        updated_lesson(),
    ])
    .await;
    run_chat(&app, "让 workspace 检查通过").await;
    wait_for_experience(&app, |entries| entries.len() == 1).await;
    wait_for_experience_inbox_settled(&app).await;
    run_chat(&app, "再次让 workspace 检查通过").await;
    wait_for_experience_inbox_settled(&app).await;
    let current = experience_views(&app);
    assert_eq!(current.len(), 1, "one current lesson: {current:?}");
    let pending = pending_experiences(&app);
    assert_eq!(pending.len(), 1, "the new evidence waits: {pending:?}");
    assert!(pending[0].body.contains("新的证据"), "{pending:?}");
    accept_experience(&app, &pending[0].id);
    assert_eq!(
        experience_views(&app).len(),
        1,
        "still one current lesson after review"
    );

    let archived = app
        .memory_service()
        .archived(
            &leveler_memory::v2::Actor::External,
            leveler_memory::ServiceScope::Project,
            leveler_memory::MemoryLanes::ALL,
            None,
        )
        .unwrap();
    assert!(
        archived
            .iter()
            .any(|entry| entry.title.contains("marker.txt")),
        "the old explanation is kept in history: {archived:?}"
    );
}

/// DF-M3: an environment change invalidates the premise the stored lesson was
/// earned under. The same failure signature must supersede the old lesson, and
/// a later session must recall only the replacement. The old lesson must not be
/// deleted: it stays archived with the value that was true when it was earned.
#[tokio::test]
async fn a_changed_environment_supersedes_the_stale_experience_and_recalls_only_the_replacement() {
    let (tmp, server, app) = harness(vec![
        tool_call("c-fail", "run_command", check_script()),
        tool_call(
            "c-fix",
            "write_file",
            serde_json::json!({"path": "engine.config", "content": "profile=legacy\n"}),
        ),
        tool_call("c-pass", "run_command", check_script()),
        text("done"),
        legacy_profile_lesson(),
        tool_call("c-fail", "run_command", check_script()),
        tool_call(
            "c-fix",
            "write_file",
            serde_json::json!({"path": "engine.config", "content": "profile=current\n"}),
        ),
        tool_call("c-pass", "run_command", check_script()),
        text("done"),
        current_profile_lesson(),
        text("current"),
        json_text(r#"{"experiences":[]}"#),
    ])
    .await;
    std::fs::write(
        workspace_file(&tmp, "check.sh"),
        "#!/bin/sh\nset -eu\nexec sh ./engine build\n",
    )
    .unwrap();
    std::fs::write(workspace_file(&tmp, "engine.config"), "profile=current\n").unwrap();
    std::fs::write(workspace_file(&tmp, "engine"), engine_script("legacy")).unwrap();

    run_chat(&app, "让 ./check.sh 通过").await;
    let first = wait_for_experience(&app, |entries| {
        entries.len() == 1 && entries[0].body.contains("legacy")
    })
    .await;
    let old_id = first[0].id.clone();
    let old_key = first[0]
        .subject_key
        .clone()
        .expect("the experience has an identity");
    assert_eq!(first[0].experience.as_ref().unwrap().standing, "verified");
    wait_for_experience_inbox_settled(&app).await;

    // The installed engine changes outside the task; the project keeps the
    // fix earned under the previous engine.
    std::fs::write(workspace_file(&tmp, "engine"), engine_script("current")).unwrap();

    run_chat(&app, "再次让 ./check.sh 通过").await;
    wait_for_experience_inbox_settled(&app).await;
    // The current lesson is the user's: an automatic writer proposes the new
    // explanation instead of rewriting it. One identity keeps one current row.
    let current = experience_views(&app);
    assert_eq!(current.len(), 1, "no duplicate current lesson: {current:?}");
    assert_eq!(current[0].id, old_id);
    let pending = pending_experiences(&app);
    assert_eq!(pending.len(), 1, "the replacement waits: {pending:?}");
    assert!(
        pending[0].body.contains("current") && pending[0].body.contains("旧环境"),
        "{pending:?}"
    );
    assert_eq!(
        pending[0].subject_key.as_deref(),
        Some(old_key.as_str()),
        "the same failure signature keeps one identity"
    );
    let new_id = pending[0].id.clone();
    accept_experience(&app, &new_id);
    assert_eq!(
        pending_experiences(&app).len(),
        0,
        "the proposal was applied"
    );
    let current = experience_views(&app);
    assert_eq!(current.len(), 1, "one current version, not two");
    assert_eq!(current[0].id, new_id, "the accepted lesson is current");
    assert_eq!(current[0].experience.as_ref().unwrap().standing, "verified");

    let archived = app
        .memory_service()
        .archived(
            &leveler_memory::v2::Actor::External,
            leveler_memory::ServiceScope::Project,
            leveler_memory::MemoryLanes::ALL,
            None,
        )
        .unwrap();
    // The attic copy is the retired revision; its own frontmatter still says
    // `active` because that is what the file said when it was current.
    let superseded = archived
        .iter()
        .find(|entry| entry.id == old_id)
        .expect("the old lesson is archived, not deleted");
    assert!(!superseded.current, "the old revision is history");
    assert_eq!(superseded.status, "archived");
    let active = app
        .memory_service()
        .listing_for_user(
            leveler_memory::ServiceScope::Project,
            leveler_memory::MemoryLanes::ALL,
        )
        .unwrap();
    assert!(active.iter().all(|entry| entry.id != old_id));
    assert_eq!(active.len(), 1, "one current version, not two");

    run_chat(
        &app,
        "为了让 ./check.sh 通过，engine.config 应该写什么 profile？",
    )
    .await;
    wait_for_experience_inbox_settled(&app).await;
    let bodies = server.request_bodies().await;
    let chat = bodies
        .iter()
        .rev()
        .find(|body| body.contains("engine.config 应该写什么 profile"))
        .expect("the third session reached the provider");
    assert!(
        chat.contains(&format!("[{new_id} r1]")),
        "the replacement must be recalled: {chat}"
    );
    assert!(
        !chat.contains(&format!("[{old_id} r1]")),
        "the superseded lesson must not be recalled: {chat}"
    );
    // Recall delivers the applicability clause and a readable handle, not the
    // body: the model expands the lesson through the memory tool.
    assert!(
        chat.contains("applies when: 项目检查脚本报告 engine profile 被拒绝"),
        "the replacement's candidate row must reach the model: {chat}"
    );
    assert!(
        !chat.contains("profile=legacy"),
        "the stale body must not reach the model: {chat}"
    );
}

#[tokio::test]
async fn a_lesson_never_replaces_a_user_decision_on_the_same_subject() {
    let (tmp, _server, app) = harness(vec![
        tool_call("c-fail", "run_command", check_marker()),
        tool_call("c-fix", "write_file", write_marker()),
        tool_call("c-pass", "run_command", check_marker()),
        text("done"),
        verified_lesson(),
    ])
    .await;
    // A user-authored decision under the same semantic identity the lesson
    // names. The runtime must not silently replace it.
    let subject = "experience.workspace.marker";
    let mut request = leveler_memory::RememberRequest::new(
        leveler_memory::v2::OpId::derived("fixture", "marker"),
        leveler_memory::ServiceScope::Project,
        "workspace 检查规则",
        "marker.txt 必须由人工创建，agent 不得自动创建。",
        "decision",
    );
    request.subject = Some(subject.to_string());
    let decision_id = app
        .memory_service()
        .remember(
            &leveler_memory::v2::Actor::User {
                invocation: leveler_memory::v2::UserInvocationId::mint("marker-rule"),
            },
            request,
        )
        .unwrap()
        .applied_id()
        .expect("the user decision is current")
        .to_string();

    run_chat(&app, "让 workspace 检查通过").await;
    wait_for_experience_inbox_settled(&app).await;
    let active = app
        .memory_service()
        .listing_for_user(
            leveler_memory::ServiceScope::Project,
            leveler_memory::MemoryLanes::ALL,
        )
        .unwrap();
    let decision = active
        .iter()
        .find(|view| view.id == decision_id)
        .expect("the user's decision is still current");
    assert_eq!(decision.authority, "user_explicit");
    assert_eq!(decision.revision, Some(1), "the decision was not rewritten");
    assert!(
        decision.body.contains("marker.txt 必须由人工创建"),
        "{decision:?}"
    );
    assert_eq!(
        active.len(),
        2,
        "the lesson lives in its own lane and the decision survives: {active:?}"
    );
    assert!(
        active.iter().any(|view| view.experience.is_some()),
        "the lesson is recorded: {active:?}"
    );
}

#[tokio::test]
async fn a_credential_in_a_command_never_reaches_the_experience_extractor() {
    let (tmp, server, app) = harness(vec![
        // A secret-bearing command fails, then the real verification-class
        // command fails twice and passes: there is a pack, but the curl line
        // must be withheld from it.
        tool_call(
            "c-secret",
            "run_command",
            serde_json::json!({
                "program": "curl",
                "args": ["-H", "Authorization: Bearer sk-live-abcdefghijklmnop"]
            }),
        ),
        tool_call("c-fail", "run_command", check_marker()),
        tool_call("c-fix", "write_file", write_marker()),
        tool_call("c-pass", "run_command", check_marker()),
        tool_call(
            "c-leak",
            "run_command",
            serde_json::json!({"program": "sh", "args": ["-c", "cat secret.txt"]}),
        ),
        text("done"),
        verified_lesson_ids("failure-2-c-fail", "resolution-4-c-pass"),
    ])
    .await;
    std::fs::write(
        workspace_file(&tmp, "secret.txt"),
        "sk-live-abcdefghijklmnopqrstuvwxyz0123",
    )
    .unwrap();
    run_chat(&app, "让 workspace 检查通过").await;
    let _entries = wait_for_experience(&app, |entries| entries.len() == 1).await;
    wait_for_experience_inbox_settled(&app).await;
    let bodies = server.request_bodies().await;
    let extraction: Vec<&String> = bodies
        .iter()
        .filter(|body| body.contains("extract durable, evidence-bound lessons"))
        .collect();
    assert!(!extraction.is_empty());
    for body in extraction {
        assert!(
            !body.contains("sk-live"),
            "a credential-bearing command reached the extractor: {body}"
        );
    }
    for entry in app
        .memory_service()
        .listing_for_user(
            leveler_memory::ServiceScope::Project,
            leveler_memory::MemoryLanes::ALL,
        )
        .unwrap()
    {
        let raw = format!("{entry:?}");
        assert!(!raw.contains("sk-live"), "{raw}");
    }
    // The Markdown files themselves must not carry it either.
    let current = tmp.path().join("workspace/.leveler/memory");
    if current.exists() {
        for file in std::fs::read_dir(&current).unwrap() {
            let text = std::fs::read_to_string(file.unwrap().path()).unwrap_or_default();
            assert!(!text.contains("sk-live"), "a credential reached Markdown");
        }
    }
}
