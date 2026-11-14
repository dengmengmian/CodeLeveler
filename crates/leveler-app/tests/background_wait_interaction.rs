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
use leveler_storage::EventStore as _;
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
    /// A resident process a service goal left running, registered exactly as
    /// its own round's `run_command(background: true)` would leave it.
    ///
    /// It belongs to a goal that is not the turn being ended, which is the case
    /// the runtime must spare when it settles its own goal: a `goal`-lifetime
    /// task owned by the very turn that ends is stopped with that turn (that is
    /// what the live PTY background contract proves). The cases below need a
    /// process that outlives the turn so a terminal fact can land later.
    async fn start_service(&self, script: &str) -> String {
        self.app
            .background_tasks()
            .spawn_with_origin(
                leveler_execution::ProcessRequest::new(
                    "sh",
                    vec!["-c".into(), script.to_string()],
                    self.repo.clone(),
                ),
                None,
                Some(self.session.as_str()),
                "parent",
                BackgroundTaskLifetime::Goal,
                leveler_execution::BackgroundTaskOrigin {
                    call_id: Some("call-start".into()),
                    goal_id: Some(leveler_core::GoalId::new("service-goal")),
                },
            )
            .await
            .expect("spawn the service")
    }

    async fn wait_for_task_exit(
        &self,
        events: &mut tokio::sync::broadcast::Receiver<leveler_execution::BackgroundTaskEvent>,
        task: &str,
    ) -> leveler_execution::BackgroundTaskSnapshot {
        tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                if let leveler_execution::BackgroundTaskEvent::Exited { task: snapshot, .. } =
                    events.recv().await.unwrap()
                    && snapshot.id == task
                {
                    break snapshot;
                }
            }
        })
        .await
        .expect("the original process reaches a terminal state")
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

    /// Block until `count` terminal facts are rows in the event log.
    ///
    /// The admission cap reads this log. A registry snapshot that has not been
    /// written yet is not the fact the next process would recover.
    async fn wait_for_durable_results(&self, count: usize) {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        loop {
            let db = self.app.open_database().await.expect("database");
            let rows = db
                .load_by_types(&self.session, &["background_task_finished"])
                .await
                .expect("load terminal facts");
            if rows.len() >= count {
                return;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "durable terminal facts stalled at {} of {count}",
                rows.len()
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    /// Wait until every input in `ids` is durably delivered to the model, and
    /// fail if the turn ends first.
    ///
    /// `InputSettled(Delivered)` is written in the same transaction that
    /// appends the input to the round it will be sent in, so it is the
    /// product's own boundary for "the text entered the model's context" —
    /// not an optimistic client row. A terminal event before both settles
    /// means one message was left for another turn, which is the defect this
    /// fixture exists to refuse.
    async fn wait_for_inputs_delivered(
        &self,
        rx: &mut tokio::sync::broadcast::Receiver<RuntimeEvent>,
        ids: &[&str],
    ) {
        use leveler_client_protocol::UiInputState;
        use tokio::sync::broadcast::error::RecvError;

        let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
        let mut delivered: Vec<&str> = Vec::new();
        loop {
            let left = deadline.saturating_duration_since(tokio::time::Instant::now());
            match tokio::time::timeout(left, rx.recv()).await {
                Ok(Ok(RuntimeEvent::InputSettled {
                    message_id,
                    state: UiInputState::Delivered,
                    ..
                })) => {
                    if let Some(id) = ids.iter().find(|id| **id == message_id.as_str())
                        && !delivered.contains(id)
                    {
                        delivered.push(*id);
                    }
                    if delivered.len() == ids.len() {
                        return;
                    }
                }
                Ok(Ok(RuntimeEvent::TurnCompleted))
                | Ok(Ok(RuntimeEvent::TurnCompletedWithWarnings { .. }))
                | Ok(Ok(RuntimeEvent::TurnAnswered))
                | Ok(Ok(RuntimeEvent::TurnPaused { .. }))
                | Ok(Ok(RuntimeEvent::TurnTruncated { .. }))
                | Ok(Ok(RuntimeEvent::TurnIncomplete { .. }))
                | Ok(Ok(RuntimeEvent::TurnBlocked { .. }))
                | Ok(Ok(RuntimeEvent::TurnFailed { .. }))
                | Ok(Ok(RuntimeEvent::TurnCancelled))
                | Ok(Ok(RuntimeEvent::TaskCancelled)) => panic!(
                    "the turn ended before every input was delivered: {delivered:?} of {ids:?}"
                ),
                Ok(Ok(_)) | Ok(Err(RecvError::Lagged(_))) => continue,
                Ok(Err(RecvError::Closed)) => {
                    panic!("the event stream closed before every input was delivered: {ids:?}")
                }
                Err(_) => panic!("inputs never reached a model request: {delivered:?} of {ids:?}"),
            }
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
            message_id: None,
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

/// Two messages in a row while the wait runs: both are delivered, in order,
/// during one turn — the second never replaces the first, and neither opens a
/// second turn.
///
/// The turn cannot answer on its own: every scripted round reads the task
/// again, and the test releases the final answer only after both inputs are
/// durably delivered. A steer therefore cannot race the end of the turn here,
/// and batching into one request is not assumed — the contract is delivery in
/// the same turn, at the round that draws the input.
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
    // The last response repeats, so the turn keeps taking rounds until the
    // test releases it: an input admitted between two rounds is drained by a
    // later round of this same turn instead of meeting a turn that ended.
    f.server
        .push(tool_call(
            "call-get",
            "get_task",
            serde_json::json!({ "task_id": task }),
        ))
        .await;

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
    let messages = [
        ("steer-1", "第一条：进展如何？"),
        ("steer-2", "第二条：先别重启服务。"),
    ];
    for (id, message) in messages {
        f.client
            .send(ClientCommand::SteerCurrentTurn {
                session_id: f.session.clone(),
                content: message.into(),
                message_id: Some(leveler_client_protocol::MessageId::new(id)),
            })
            .await
            .unwrap();
    }
    f.wait_for_inputs_delivered(&mut rx, &["steer-1", "steer-2"])
        .await;
    f.server.push(text("两条追问都已收到。")).await;
    f.wait_for_turn_end(&mut rx).await;

    let bodies = f.server.request_bodies().await;
    let position = |needle: &str| bodies.iter().position(|body| body.contains(needle));
    let first = position(messages[0].1);
    let second = position(messages[1].1);
    assert!(
        first.is_some(),
        "the first message never reached a model request; bodies={bodies:?}"
    );
    assert!(
        second.is_some(),
        "the second message never reached a model request; bodies={bodies:?}"
    );
    assert!(
        first.unwrap() <= second.unwrap(),
        "the messages keep their order: {first:?} then {second:?}"
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
    let mut background = f.app.background_tasks().subscribe();
    // The exit and the steer must land together, but a wall-clock sleep makes
    // that a machine-speed race. The service waits for the trigger the test
    // writes immediately after the steer, so both facts are in flight at once.
    let task = f
        .start_service(
            "while [ ! -f exit-flag ]; do sleep 0.05; done; printf 'tests done\\n'; exit 0",
        )
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
    // Spawn the steer and release the exit in the same tick. Awaiting the
    // steer first lets the runtime finish its handoff before the trigger is
    // visible, which turns a tie into "steer first" for reasons unrelated to
    // the contract under test.
    let steer = tokio::spawn({
        let client = f.client.clone();
        let session = f.session.clone();
        async move {
            client
                .send(ClientCommand::SteerCurrentTurn {
                    session_id: session,
                    content: "进展如何？".into(),
                    message_id: None,
                })
                .await
        }
    });
    std::fs::write(f.repo.join("exit-flag"), b"").unwrap();
    steer.await.unwrap().unwrap();
    f.wait_for_turn_end(&mut rx).await;
    let terminal = f.wait_for_task_exit(&mut background, &task).await;
    assert_eq!(terminal.exit_code, Some(0));
    assert!(terminal.log.contains("tests done"));
    // Always admit another real model request after the terminal event. The
    // steered round can legitimately have taken its context snapshot first.
    // The following request must carry the result either way, without the
    // model issuing a second wait, and the steer must also have arrived.
    f.server.push(text("已核对结果。")).await;
    f.client
        .send(ClientCommand::SubmitMessage {
            session_id: f.session.clone(),
            content: "核对完成结果。".into(),
            attachments: vec![],
        })
        .await
        .unwrap();
    f.wait_for_turn_end(&mut rx).await;
    let bodies = f.server.request_bodies().await;
    assert!(bodies.last().unwrap().contains("tests done"));
    f.wait_for_request_containing("进展如何").await;
    let snapshots = f.app.background_tasks().try_all_snapshots().await.unwrap();
    assert_eq!(snapshots.len(), 1, "no second task was started");
}

/// The process cannot exit until the first steered turn has answered. Its
/// eventual result must reach a new request without another observation tool.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_terminal_fact_after_the_steered_turn_reaches_the_next_request() {
    let f = fixture().await;
    let mut background = f.app.background_tasks().subscribe();
    let task = f
        .start_service(
            "while [ ! -f exit-flag ]; do sleep 0.05; done; printf 'late exit marker\\n'; exit 7",
        )
        .await;
    let mut second_background = f.app.background_tasks().subscribe();
    let second = f
        .start_service(
            "while [ ! -f exit-flag ]; do sleep 0.05; done; printf 'second exit marker\\n'; exit 0",
        )
        .await;
    f.server
        .push(tool_call(
            "call-wait",
            "wait_task",
            serde_json::json!({ "task_id": task }),
        ))
        .await;
    f.server.push(text("Still running.")).await;
    f.server.push(text("The task exited.")).await;
    f.server.push(text("The result is already recorded.")).await;
    f.server
        .push(MockResponse::json_ok(
            &serde_json::json!({
                "id": "compact-response",
                "choices": [{
                    "message": {"role": "assistant", "content": "Compacted conversation."},
                    "finish_reason": "stop"
                }]
            })
            .to_string(),
        ))
        .await;
    f.server.push(text("Continue.")).await;
    let mut rx = f.client.subscribe();
    f.client
        .send(ClientCommand::SubmitMessage {
            session_id: f.session.clone(),
            content: "Start and verify.".into(),
            attachments: vec![],
        })
        .await
        .unwrap();
    f.wait_for_tool_call(&mut rx, "wait_task").await;
    f.client
        .send(ClientCommand::SteerCurrentTurn {
            session_id: f.session.clone(),
            content: "Status?".into(),
            message_id: None,
        })
        .await
        .unwrap();
    f.wait_for_turn_end(&mut rx).await;
    std::fs::write(f.repo.join("exit-flag"), b"").unwrap();
    let (snapshot, second_snapshot) = tokio::join!(
        f.wait_for_task_exit(&mut background, &task),
        f.wait_for_task_exit(&mut second_background, &second),
    );
    assert_eq!(snapshot.exit_code, Some(7));
    assert!(snapshot.log.contains("late exit marker"));
    assert_eq!(second_snapshot.exit_code, Some(0));
    assert!(second_snapshot.log.contains("second exit marker"));

    f.client
        .send(ClientCommand::SubmitMessage {
            session_id: f.session.clone(),
            content: "Continue checking the result.".into(),
            attachments: vec![],
        })
        .await
        .unwrap();
    f.wait_for_turn_end(&mut rx).await;
    let bodies = f.server.request_bodies().await;
    let request = bodies.last().expect("the next turn requested a model");
    assert!(request.contains("late exit marker"));
    assert!(request.contains(&task));
    assert!(request.contains("second exit marker"));
    assert!(request.contains(&second));

    f.client
        .send(ClientCommand::SubmitMessage {
            session_id: f.session.clone(),
            content: "Review the report.".into(),
            attachments: vec![],
        })
        .await
        .unwrap();
    f.wait_for_turn_end(&mut rx).await;
    let bodies = f.server.request_bodies().await;
    let request = bodies.last().expect("another model request was admitted");
    assert_eq!(
        request.matches("Background task terminal result:").count(),
        2
    );

    f.client
        .send(ClientCommand::CompactContext {
            session_id: f.session.clone(),
        })
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            match rx.recv().await.unwrap() {
                RuntimeEvent::ContextCompacted { .. } => break,
                RuntimeEvent::TurnFailed { error, .. } => panic!("compaction failed: {error}"),
                _ => {}
            }
        }
    })
    .await
    .expect("compaction completes");
    let db = f.app.open_database().await.unwrap();
    let transcript = leveler_storage::MessageRepository::new(&db)
        .load(&f.session)
        .await
        .unwrap();
    assert_eq!(
        transcript.len(),
        1,
        "compaction really replaced the transcript"
    );

    f.client
        .send(ClientCommand::SubmitMessage {
            session_id: f.session.clone(),
            content: "Continue with the summary.".into(),
            attachments: vec![],
        })
        .await
        .unwrap();
    f.wait_for_turn_end(&mut rx).await;
    f.wait_for_request_containing("Continue with the summary.")
        .await;
    let bodies = f.server.request_bodies().await;
    let request = bodies.last().unwrap();
    assert!(request.contains("Compacted conversation."));
    assert!(!request.contains("Background task terminal result:"));
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
            message_id: None,
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
            message_id: None,
        })
        .await
        .unwrap();

    f.wait_for_request_containing("最后一句：结果如何").await;
    f.app.background_tasks().kill(&task).await.ok();
}

fn terminal_result_count(body: &str) -> usize {
    body.matches("Background task terminal result:").count()
}

/// Nine finished tasks are waiting before any model request. One admission
/// carries eight notices. The ninth reaches the next admission, and a third
/// admission does not mint any of them again.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn nine_undelivered_terminal_facts_drain_without_dropping_the_last() {
    let f = fixture().await;
    // The session bridge is what writes the exit into the event log. It has
    // to be listening before the processes finish.
    let _events = f.client.subscribe_session(&f.session);
    // The registry admits four active tasks. These facts only have to be
    // terminal and undelivered, so each one finishes before the next starts.
    // A burst of nine hits `MAX_CONCURRENT` on a host that reaps slower.
    for index in 0..9 {
        let id = f
            .start_service(&format!("printf 'FACT9_{index:02}\\n'; exit 0"))
            .await;
        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        loop {
            let snapshots = f.app.background_tasks().try_all_snapshots().await.unwrap();
            let settled = snapshots
                .iter()
                .any(|task| task.id == id && task.status.is_terminal());
            if settled {
                break;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "spawned task {id} never reached a terminal snapshot"
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }
    f.wait_for_durable_results(9).await;

    let mut rx = f.client.subscribe();
    for (label, expect_notices) in [
        ("第一轮核对。", 8),
        ("第二轮核对。", 9),
        ("第三轮核对。", 9),
    ] {
        f.server.push(text("已记录。")).await;
        f.client
            .send(ClientCommand::SubmitMessage {
                session_id: f.session.clone(),
                content: label.into(),
                attachments: vec![],
            })
            .await
            .unwrap();
        f.wait_for_turn_end(&mut rx).await;
        let bodies = f.server.request_bodies().await;
        let request = bodies
            .iter()
            .rev()
            .find(|body| body.contains(label))
            .expect("the admission was sent");
        assert_eq!(
            terminal_result_count(request),
            expect_notices,
            "{label} carried the wrong number of terminal notices"
        );
    }
    let bodies = f.server.request_bodies().await;
    let first = bodies
        .iter()
        .find(|body| body.contains("第一轮核对。"))
        .unwrap();
    let second = bodies
        .iter()
        .find(|body| body.contains("第二轮核对。"))
        .unwrap();
    let deferred = (0..9)
        .find(|index| !first.contains(&format!("FACT9_{index:02}")))
        .expect("the first admission must leave one fact for the next one");
    assert!(
        second.contains(&format!("FACT9_{deferred:02}")),
        "deferred fact FACT9_{deferred:02} never reached the next request"
    );
    let third = bodies
        .iter()
        .find(|body| body.contains("第三轮核对。"))
        .unwrap();
    for index in 0..9 {
        assert!(
            third.contains(&format!("FACT9_{index:02}")),
            "FACT9_{index:02} was dropped from the transcript"
        );
    }
}

/// The fact is already inside the request that failed. The retry sends that
/// same request, and the following turn does not drop the fact or inject it
/// a second time.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_provider_failure_retries_the_request_that_already_holds_the_fact() {
    let f = fixture().await;
    let _events = f.client.subscribe_session(&f.session);
    let id = f
        .start_service("printf 'provider-retry-marker\\n'; exit 0")
        .await;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        let snapshots = f.app.background_tasks().try_all_snapshots().await.unwrap();
        if snapshots
            .iter()
            .any(|task| task.id == id && task.status.is_terminal())
        {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "the task never exited"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    f.wait_for_durable_results(1).await;

    f.server
        .push(MockResponse::TruncatedBody {
            body: String::new(),
            content_type: "text/event-stream".into(),
        })
        .await;
    f.server.push(text("已恢复。")).await;

    let mut rx = f.client.subscribe();
    f.client
        .send(ClientCommand::SubmitMessage {
            session_id: f.session.clone(),
            content: "看刚才的结果。".into(),
            attachments: vec![],
        })
        .await
        .unwrap();
    f.wait_for_turn_end(&mut rx).await;

    let bodies = f.server.request_bodies().await;
    assert!(
        bodies.len() >= 2,
        "an empty provider body must be retried, saw {}",
        bodies.len()
    );
    assert!(
        bodies[0].contains("provider-retry-marker"),
        "the failed request was sent without the terminal fact"
    );
    assert!(
        bodies[1].contains("provider-retry-marker"),
        "the retry dropped the terminal fact"
    );
    assert_eq!(
        terminal_result_count(&bodies[0]),
        terminal_result_count(&bodies[1]),
        "the retry was not the same admitted request"
    );

    f.server.push(text("没有重复。")).await;
    f.client
        .send(ClientCommand::SubmitMessage {
            session_id: f.session.clone(),
            content: "再核对一次。".into(),
            attachments: vec![],
        })
        .await
        .unwrap();
    f.wait_for_turn_end(&mut rx).await;
    let bodies = f.server.request_bodies().await;
    let last = bodies
        .iter()
        .rev()
        .find(|body| body.contains("再核对一次。"))
        .expect("the follow-up admission was sent");
    assert_eq!(
        terminal_result_count(last),
        1,
        "the fact was injected again after the failed request"
    );
    assert!(last.contains("provider-retry-marker"));
}
