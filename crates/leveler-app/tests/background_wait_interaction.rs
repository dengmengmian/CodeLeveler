//! The user's message during a background wait, through the real runtime.
//!
//! The reported defect: the model waits for a resident service to exit, the
//! person types "现在进展怎么样了？", the runtime queues the text — and the turn
//! keeps holding the wait for its own bound. Enqueued is not answered.
//!
//! These tests drive the real `InProcessRuntimeClient`, the real steer command,
//! the real tool registry and a real background process. The model is scripted
//! over the real provider wire, so what is proven here is the runtime's
//! scheduling, not a model's cooperation.
#![cfg(unix)]

use std::sync::Arc;
use std::time::Duration;

use leveler_app::{Application, InProcessRuntimeClient};
use leveler_client_protocol::{ClientCommand, InteractiveRuntimeClient, RuntimeEvent};
use leveler_execution::{BackgroundTaskLifetime, PermissionProfile};
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

fn tool_call(id: &str, name: &str, arguments: serde_json::Value) -> MockResponse {
    sse(vec![
        serde_json::json!({"choices": [{"delta": {"tool_calls": [{
            "index": 0, "id": id,
            "function": {"name": name, "arguments": arguments.to_string()}
        }]}}]})
        .to_string(),
        serde_json::json!({"choices": [{"delta": {}, "finish_reason": "tool_calls"}]}).to_string(),
    ])
}

fn text(content: &str) -> MockResponse {
    sse(vec![
        serde_json::json!({"choices": [{"delta": {"content": content}, "finish_reason": "stop"}]})
            .to_string(),
    ])
}

struct Fixture {
    _tmp: tempfile::TempDir,
    server: MockServer,
    client: Arc<InProcessRuntimeClient>,
    session: leveler_core::SessionId,
    app: Arc<Application>,
    repo: std::path::PathBuf,
}

async fn fixture() -> Fixture {
    isolate_global_config();
    // The script depends on the task id the runtime assigns, so it is pushed
    // once that id exists (see [`Fixture::start_service`]).
    let server = MockServer::start(Vec::new()).await;
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
limits: { context_window: 131072, reliable_context: 65536, max_output_tokens: 1024, max_tool_schema_bytes: 8192, max_parallel_tool_calls: 1 }
compatibility: { synthesize_tool_call_ids: true, drop_unsupported_fields: true }
"#,
    )
    .unwrap();
    let layout = Layout::from_parts(
        tmp.path().to_path_buf(),
        tmp.path().join("configs"),
        tmp.path().join("state"),
    );
    let app = Arc::new(
        Application::assemble(layout)
            .unwrap()
            // One ordinary exchange: this test is about the wait, not about the
            // goal continuation policy.
            .with_collaboration(leveler_agent::CollaborationMode::Chat),
    );
    let model = ModelRef::new("mock", "m");
    let session = app.create_session(&model, "goal").await.unwrap();
    let client = Arc::new(InProcessRuntimeClient::new(
        app.clone(),
        model,
        PermissionProfile::FullAccess,
        false,
    ));
    let repo = tmp.path().to_path_buf();
    Fixture {
        _tmp: tmp,
        server,
        client,
        session,
        app,
        repo,
    }
}

impl Fixture {
    /// A resident process, registered exactly as a previous round's
    /// `run_command(background: true)` would leave it.
    async fn start_service(&self, script: &str) -> String {
        self.app
            .background_tasks()
            .spawn_for_writer(
                leveler_execution::ProcessRequest::new(
                    "sh",
                    vec!["-c".into(), script.to_string()],
                    self.repo.clone(),
                ),
                None,
                Some(self.session.as_str()),
                "parent",
                BackgroundTaskLifetime::Goal,
                Some("call-start".into()),
            )
            .await
            .expect("spawn the service")
    }

    /// Wait until a call of `name` is announced, then report when.
    async fn wait_for_tool_call(
        &self,
        rx: &mut tokio::sync::broadcast::Receiver<RuntimeEvent>,
        name: &str,
    ) -> tokio::time::Instant {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
        loop {
            let left = deadline.saturating_duration_since(tokio::time::Instant::now());
            let event = tokio::time::timeout(left, rx.recv())
                .await
                .unwrap_or_else(|_| panic!("`{name}` never started"))
                .expect("event stream open");
            if let RuntimeEvent::ToolCallStarted { name: started, .. } = event
                && started == name
            {
                return tokio::time::Instant::now();
            }
        }
    }

    /// Wait until a model request carries `needle` — the only sound way to say
    /// the runtime actually delivered something to the model.
    async fn wait_for_request_containing(&self, needle: &str) {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
        loop {
            let bodies = self.server.request_bodies().await;
            if bodies.iter().any(|body| body.contains(needle)) {
                return;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "no model request ever carried {needle:?}: {} bodies; last={:?}",
                bodies.len(),
                bodies
                    .last()
                    .map(|body| &body[body.len().saturating_sub(600)..])
            );
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    }

    async fn wait_for_turn_end(&self, rx: &mut tokio::sync::broadcast::Receiver<RuntimeEvent>) {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
        loop {
            let left = deadline.saturating_duration_since(tokio::time::Instant::now());
            match tokio::time::timeout(left, rx.recv()).await {
                Ok(Ok(RuntimeEvent::TurnCompleted))
                | Ok(Ok(RuntimeEvent::TurnAnswered))
                | Ok(Ok(RuntimeEvent::TurnFailed { .. }))
                | Ok(Ok(RuntimeEvent::TurnCancelled)) => return,
                Ok(Ok(_)) | Ok(Err(tokio::sync::broadcast::error::RecvError::Lagged(_))) => {
                    continue;
                }
                Ok(Err(_)) => return,
                Err(_) => panic!("the turn never ended"),
            }
        }
    }
}

/// The defect itself: a steer sent while `wait_task` holds the round must be
/// answered by the same turn, while the process keeps running.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_steer_during_a_background_wait_is_answered_without_ending_the_task() {
    let f = fixture().await;
    let task = f.start_service("printf 'booting\\n'; sleep 120").await;
    f.server
        .push(tool_call(
            "call-wait",
            "wait_task",
            serde_json::json!({ "task_id": task, "timeout_seconds": 600 }),
        ))
        .await;
    f.server
        .push(tool_call(
            "call-get",
            "get_task",
            serde_json::json!({ "task_id": task }),
        ))
        .await;
    f.server
        .push(text("服务仍在运行，还没有就绪证据；我先看日志再继续。"))
        .await;

    let mut rx = f.client.subscribe();
    f.client
        .send(ClientCommand::SubmitMessage {
            session_id: f.session.clone(),
            content: "本地启动项目，然后执行完整测试。".into(),
            attachments: vec![],
        })
        .await
        .unwrap();
    let steered_at = f.wait_for_tool_call(&mut rx, "wait_task").await;
    f.client
        .send(ClientCommand::SteerCurrentTurn {
            session_id: f.session.clone(),
            content: "现在进展怎么样了？".into(),
        })
        .await
        .unwrap();
    f.wait_for_turn_end(&mut rx).await;
    let elapsed = steered_at.elapsed();
    assert!(
        elapsed < Duration::from_secs(15),
        "the steer waited {elapsed:?} for the wait's own bound"
    );

    // The service was not stopped and not restarted: exactly one task record,
    // and the model's own mid-turn read of it reports a live process. (The
    // record itself is session-owned `goal` lifetime, so the engine's terminal
    // settlement reaps it after this turn — that is the documented contract,
    // not this test's subject.)
    let snapshots = f.app.background_tasks().try_all_snapshots().await.unwrap();
    assert_eq!(snapshots.len(), 1, "the same task, not a duplicate");
    assert_eq!(snapshots[0].id, task);

    // The runtime's own observation and the user's text both reached the model.
    let bodies = f.server.request_bodies().await;
    assert!(
        bodies.iter().skip(1).any(|body| body.contains(&task)),
        "the model was given the task it is observing"
    );
    assert!(
        bodies.iter().any(|body| body.contains("status: running")),
        "the model read a live process while answering the user"
    );
    assert!(
        bodies
            .iter()
            .skip(1)
            .any(|body| body.contains("现在进展怎么样了")),
        "the steer never reached a model request"
    );
    assert!(
        bodies.iter().any(|body| body.contains("call-get")),
        "the model could read the task before answering"
    );

    f.app.background_tasks().kill(&task).await.ok();
}

/// Two messages in a row while the wait runs: both are delivered, in order, to
/// one turn — the second never replaces the first, and neither opens a second
/// turn.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn two_steers_during_a_wait_both_reach_the_same_turn() {
    let f = fixture().await;
    let task = f.start_service("sleep 120").await;
    f.server
        .push(tool_call(
            "call-wait",
            "wait_task",
            serde_json::json!({ "task_id": task, "timeout_seconds": 600 }),
        ))
        .await;
    f.server.push(text("第一条追问的答复。")).await;
    f.server.push(text("第二条追问的答复。")).await;

    let mut rx = f.client.subscribe();
    f.client
        .send(ClientCommand::SubmitMessage {
            session_id: f.session.clone(),
            content: "启动服务并测试".into(),
            attachments: vec![],
        })
        .await
        .unwrap();
    f.wait_for_tool_call(&mut rx, "wait_task").await;
    for message in ["第一条：进展如何？", "第二条：先别重启服务。"] {
        f.client
            .send(ClientCommand::SteerCurrentTurn {
                session_id: f.session.clone(),
                content: message.into(),
            })
            .await
            .unwrap();
    }
    f.wait_for_turn_end(&mut rx).await;

    let bodies = f.server.request_bodies().await;
    let delivered: Vec<&String> = bodies
        .iter()
        .filter(|body| body.contains("第一条：进展如何") || body.contains("第二条：先别重启服务"))
        .collect();
    assert!(
        delivered
            .last()
            .is_some_and(|body| body.contains("第一条") && body.contains("第二条")),
        "both messages reach one model request: {}",
        delivered.len()
    );
    let snapshots = f.app.background_tasks().try_all_snapshots().await.unwrap();
    assert_eq!(snapshots.len(), 1, "no duplicate task was started");
    assert_eq!(snapshots[0].id, task);
    f.app.background_tasks().kill(&task).await.ok();
}

/// The task exits at the same instant the user types: the exit is the result
/// the model must see, and the message must still be delivered.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_exit_racing_a_steer_keeps_the_result_and_the_message() {
    let f = fixture().await;
    let task = f
        .start_service("sleep 1; printf 'tests done\\n'; exit 0")
        .await;
    f.server
        .push(tool_call(
            "call-wait",
            "wait_task",
            serde_json::json!({ "task_id": task, "timeout_seconds": 600 }),
        ))
        .await;
    f.server.push(text("测试已完成。")).await;

    let mut rx = f.client.subscribe();
    f.client
        .send(ClientCommand::SubmitMessage {
            session_id: f.session.clone(),
            content: "启动并测试".into(),
            attachments: vec![],
        })
        .await
        .unwrap();
    f.wait_for_tool_call(&mut rx, "wait_task").await;
    // Exactly inside the window where the process ends by itself.
    tokio::time::sleep(Duration::from_millis(1000)).await;
    f.client
        .send(ClientCommand::SteerCurrentTurn {
            session_id: f.session.clone(),
            content: "进展如何？".into(),
        })
        .await
        .unwrap();
    f.wait_for_turn_end(&mut rx).await;
    // Whether the exit or the steer wins the race, both facts must reach the
    // model: the exit as its result, the steer as the user's own words.
    f.wait_for_request_containing("tests done").await;
    f.wait_for_request_containing("进展如何").await;
    let snapshots = f.app.background_tasks().try_all_snapshots().await.unwrap();
    assert_eq!(snapshots.len(), 1, "no second task was started");
}

/// A message that arrives while the turn is already finalizing has no round
/// left to consume it. It is still the user's message: it must be delivered as
/// the next turn instead of sitting in a queue nobody drains.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_steer_landing_after_the_last_round_is_still_delivered() {
    let f = fixture().await;
    let task = f.start_service("sleep 120").await;
    f.server
        .push(tool_call(
            "call-wait",
            "wait_task",
            serde_json::json!({ "task_id": task, "timeout_seconds": 600 }),
        ))
        .await;
    f.server
        .push(text("服务仍在运行，已经回答你的问题。"))
        .await;

    let mut rx = f.client.subscribe();
    f.client
        .send(ClientCommand::SubmitMessage {
            session_id: f.session.clone(),
            content: "启动服务".into(),
            attachments: vec![],
        })
        .await
        .unwrap();
    f.wait_for_tool_call(&mut rx, "wait_task").await;
    f.client
        .send(ClientCommand::SteerCurrentTurn {
            session_id: f.session.clone(),
            content: "现在进展怎么样了？".into(),
        })
        .await
        .unwrap();

    // The last round has already started; the next thing the runtime does is
    // settle the turn. This is the window where a queued message used to be
    // stranded.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
    loop {
        let left = deadline.saturating_duration_since(tokio::time::Instant::now());
        let event = tokio::time::timeout(left, rx.recv())
            .await
            .expect("the turn finalizes")
            .expect("event stream open");
        match event {
            RuntimeEvent::TurnFinalizing { .. } => break,
            RuntimeEvent::TurnCompleted | RuntimeEvent::TurnAnswered => {
                // Finalization was faster than the event; deliver the steer the
                // same way the client's own fallback does.
                break;
            }
            _ => continue,
        }
    }
    f.client
        .send(ClientCommand::SteerCurrentTurn {
            session_id: f.session.clone(),
            content: "最后一句：结果如何？".into(),
        })
        .await
        .unwrap();

    f.wait_for_request_containing("最后一句：结果如何").await;
    f.app.background_tasks().kill(&task).await.ok();
}
