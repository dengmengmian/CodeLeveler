//! `/context` is a local inspection command: it answers from the runtime's
//! cached accounting and must never reach the model request. This drives two
//! real turns through the in-process client against a recording mock server,
//! asks for the context in between, and reads the exact bodies the provider
//! received.

use std::sync::Arc;
use std::time::Duration;

use leveler_app::{Application, InProcessRuntimeClient};
use leveler_client_protocol::{ClientCommand, InteractiveRuntimeClient, RuntimeEvent};
use leveler_core::{CommandId, SessionId};
use leveler_execution::PermissionProfile;
use leveler_model::ModelRef;
use leveler_project::Layout;
use leveler_storage::MessageRepository;
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

fn text(content: &str) -> MockResponse {
    sse(vec![
        serde_json::json!({"choices": [{"delta": {"content": content}, "finish_reason": "stop"}]})
            .to_string(),
    ])
}

async fn harness(
    responses: Vec<MockResponse>,
) -> (
    tempfile::TempDir,
    MockServer,
    Arc<Application>,
    Arc<InProcessRuntimeClient>,
    SessionId,
) {
    harness_with_window(responses, 8192, 4096).await
}

async fn harness_with_window(
    responses: Vec<MockResponse>,
    window: u32,
    reliable: u32,
) -> (
    tempfile::TempDir,
    MockServer,
    Arc<Application>,
    Arc<InProcessRuntimeClient>,
    SessionId,
) {
    harness_with_model(
        responses,
        window,
        reliable,
        "capabilities: { streaming: true, tool_calling: true, parallel_tool_calls: false, \
         structured_output: true, reasoning: false, vision: false }",
        "compatibility: { synthesize_tool_call_ids: true, drop_unsupported_fields: true }",
    )
    .await
}

/// The same harness with the model's declared capabilities/reasoning/compatibility
/// spelled by the caller, so a test can pin a route fact (for example a
/// reasoning-replay contract) without a second application setup.
async fn harness_with_model(
    responses: Vec<MockResponse>,
    window: u32,
    reliable: u32,
    capabilities: &str,
    compatibility: &str,
) -> (
    tempfile::TempDir,
    MockServer,
    Arc<Application>,
    Arc<InProcessRuntimeClient>,
    SessionId,
) {
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
        format!(r#"
id: m
provider: mock
model_id: mock-model
protocol: openai_chat
{capabilities}
limits: {{ context_window: {window}, reliable_context: {reliable}, max_output_tokens: 1024, max_tool_schema_bytes: 8192, max_parallel_tool_calls: 1 }}
{compatibility}
"#),
    )
    .unwrap();
    let layout = Layout::from_parts(
        tmp.path().to_path_buf(),
        tmp.path().join("configs"),
        tmp.path().join("state"),
    );
    let app = Arc::new(Application::assemble(layout).unwrap());
    let model = ModelRef::new("mock", "m");
    let session = app.create_session(&model, "goal").await.unwrap();
    let client = Arc::new(InProcessRuntimeClient::new(
        app.clone(),
        model,
        PermissionProfile::Assisted,
        false,
    ));
    (tmp, server, app, client, session)
}

#[tokio::test]
async fn rejected_manual_summaries_preserve_history_and_record_the_attempt() {
    use leveler_storage::{GoalCheckpointStore, GoalStore, OwnershipStore, TaskStore};
    for (content, finish) in [
        ("", "stop"),
        ("partial", "length"),
        ("filtered", "content_filter"),
    ] {
        let response = MockResponse::SilentThenJson {
            silent_ms: 0,
            body: serde_json::json!({
                "id": "compact-attempt", "choices": [{
                    "message": {"role": "assistant", "content": content},
                    "finish_reason": finish
                }], "usage": {"prompt_tokens": 20, "completion_tokens": 3}
            })
            .to_string(),
        };
        let (_tmp, _server, app, client, session) = harness(vec![response]).await;
        let db = app.open_database().await.unwrap();
        let task = db
            .ensure_for_session(&session, leveler_core::now())
            .await
            .unwrap();
        let owner = db
            .acquire(
                &task,
                &leveler_core::RuntimeId::new("compact-test"),
                &leveler_core::BootId::new("compact-test"),
                leveler_core::OwnerEpoch::UNOWNED,
            )
            .await
            .unwrap();
        let goal = GoalStore::open(&db, &owner, "preserve this goal", leveler_core::now())
            .await
            .unwrap();
        let scope = goal.to_string();
        let turn_payload = serde_json::json!({
            "version": 2,
            "initiating_message": leveler_model::Message::text(leveler_model::Role::User, "preserve this goal"),
            "objective": leveler_lifecycle::ObjectiveAnchor::from_session_goal("preserve this goal"),
            "continuation_root_turn_id": null,
            "goal_id": goal
        }).to_string();
        leveler_storage::TurnStore::start_owned(
            &db,
            &owner,
            &session,
            "user",
            Some(&turn_payload),
            leveler_core::now(),
        )
        .await
        .unwrap();
        let checkpoint = db
            .create(
                leveler_storage::NewGoalCheckpoint {
                    goal_id: goal,
                    session_id: session.clone(),
                    reason: leveler_lifecycle::CheckpointReason::Manual,
                    event_cursor: 0,
                    payload: leveler_lifecycle::GoalCheckpoint::default(),
                },
                leveler_core::now(),
            )
            .await
            .unwrap();
        db.release(&owner).await.unwrap();
        let repo = MessageRepository::new(&db);
        let history: Vec<_> = (0..4)
            .map(|n| {
                serde_json::to_string(&leveler_model::Message::text(
                    leveler_model::Role::User,
                    format!("original {n}"),
                ))
                .unwrap()
            })
            .collect();
        repo.append(&session, &history, leveler_core::now())
            .await
            .unwrap();
        let history = repo.load(&session).await.unwrap();
        let mut rx = client.subscribe();
        client
            .send(ClientCommand::CompactContext {
                session_id: session.clone(),
            })
            .await
            .unwrap();
        tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                if matches!(rx.recv().await.unwrap(), RuntimeEvent::TurnFailed { .. }) {
                    break;
                }
            }
        })
        .await
        .expect("invalid compact must report failure");
        assert_eq!(repo.load(&session).await.unwrap(), history);
        assert_eq!(
            GoalCheckpointStore::get(&db, &checkpoint.id).await.unwrap(),
            Some(checkpoint)
        );
        let records = leveler_storage::ModelRequestRepository::new(&db)
            .load_for_session(&session)
            .await
            .unwrap();
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].kind, leveler_storage::ModelCallKind::Compaction);
        assert_eq!(records[0].input_tokens, 20);
        assert_eq!(records[0].budget_scope.as_deref(), Some(scope.as_str()));
    }
}

/// Manual compaction shares the existing task cap even between turns.
#[tokio::test]
async fn manual_compact_respects_the_existing_task_budget_before_calling_provider() {
    let (tmp, server, app, client, session) = harness(vec![]).await;
    std::fs::create_dir_all(tmp.path().join(".leveler")).unwrap();
    std::fs::write(
        tmp.path().join(".leveler/config.yaml"),
        "limits:\n  max_model_tokens: 2000\n",
    )
    .unwrap();
    let db = app.open_database().await.unwrap();
    let payload = serde_json::json!({
        "version": 2,
        "initiating_message": leveler_model::Message::text(leveler_model::Role::User, "task"),
        "objective": leveler_lifecycle::ObjectiveAnchor::from_user_message("task"),
        "continuation_root_turn_id": null,
        "goal_id": null
    })
    .to_string();
    let turn = leveler_storage::TurnStore::start(
        &db,
        &session,
        "chat",
        Some(&payload),
        leveler_core::now(),
    )
    .await
    .unwrap();
    let progress = leveler_engine::EngineEvent::ProgressUpdated {
        ledger: leveler_lifecycle::ProgressLedger {
            budget_scope: Some(turn.id),
            cumulative_model_tokens: 1900,
            ..Default::default()
        },
    };
    let (tag, payload) = progress.to_row().unwrap();
    leveler_storage::EventRepository::new(&db)
        .append(&session, None, &tag, &payload, leveler_core::now())
        .await
        .unwrap();
    let history: Vec<_> = (0..4)
        .map(|n| {
            serde_json::to_string(&leveler_model::Message::text(
                leveler_model::Role::User,
                format!("original {n}"),
            ))
            .unwrap()
        })
        .collect();
    let repo = MessageRepository::new(&db);
    repo.append(&session, &history, leveler_core::now())
        .await
        .unwrap();
    let before = repo.load(&session).await.unwrap();
    let mut rx = client.subscribe();
    client
        .send(ClientCommand::CompactContext {
            session_id: session.clone(),
        })
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            if let RuntimeEvent::TurnFailed { error, .. } = rx.recv().await.unwrap() {
                assert!(error.contains("预算"), "{error}");
                break;
            }
        }
    })
    .await
    .unwrap();
    assert_eq!(server.request_count(), 0);
    assert_eq!(repo.load(&session).await.unwrap(), before);
    assert!(
        leveler_storage::ModelRequestRepository::new(&db)
            .load_for_session(&session)
            .await
            .unwrap()
            .is_empty()
    );
}

/// Drain events until the turn reaches a terminal state.
async fn wait_for_turn_end(rx: &mut tokio::sync::broadcast::Receiver<RuntimeEvent>) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    loop {
        let left = deadline.saturating_duration_since(tokio::time::Instant::now());
        match tokio::time::timeout(left, rx.recv()).await {
            Ok(Ok(event)) => match event {
                RuntimeEvent::TurnCompleted
                | RuntimeEvent::TurnCompletedWithWarnings { .. }
                | RuntimeEvent::TurnAnswered
                | RuntimeEvent::TurnTruncated { .. }
                | RuntimeEvent::TurnIncomplete { .. }
                | RuntimeEvent::TurnFailed { .. }
                | RuntimeEvent::TurnCancelled => return,
                _ => continue,
            },
            Ok(Err(tokio::sync::broadcast::error::RecvError::Lagged(_))) => continue,
            _ => panic!("the turn never settled"),
        }
    }
}

/// Ask for `/context` and read the answer the runtime cached from the last
/// request it assembled.
async fn query_context(
    client: &Arc<InProcessRuntimeClient>,
    rx: &mut tokio::sync::broadcast::Receiver<RuntimeEvent>,
    session: &SessionId,
) -> leveler_model::ContextAccounting {
    let query_id = CommandId::generate();
    client
        .send(ClientCommand::QueryContext {
            session_id: session.clone(),
            query_id: Some(query_id.clone()),
        })
        .await
        .unwrap();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        let left = deadline.saturating_duration_since(tokio::time::Instant::now());
        match tokio::time::timeout(left, rx.recv()).await {
            Ok(Ok(RuntimeEvent::ContextLoaded {
                accounting: Some(accounting),
                ..
            })) => return accounting,
            Ok(Ok(_)) | Ok(Err(tokio::sync::broadcast::error::RecvError::Lagged(_))) => continue,
            _ => panic!("the context query was never answered"),
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn context_inspection_is_local_and_never_enters_the_conversation() {
    // A window large enough for the harness's fixed surface (tool schemas plus
    // control) to leave a legal request: at 8192 the surface alone exceeds the
    // model's hard capacity, so no turn could start.
    let (_tmp, server, app, client, session) =
        harness_with_window(vec![text("first"), text("second")], 128_000, 100_000).await;
    let mut rx = client.subscribe();

    client
        .send(ClientCommand::SubmitMessage {
            session_id: session.clone(),
            content: "first question".into(),
            attachments: vec![],
        })
        .await
        .unwrap();
    wait_for_turn_end(&mut rx).await;

    // The runtime published the accounting of the request it just sent.
    let accounting = query_context(&client, &mut rx, &session).await;
    assert!(
        accounting.used_tokens > 0,
        "an assembled request must account for something"
    );
    assert_eq!(accounting.context_window_tokens, Some(128_000));

    // `/context` produced no model request at all.
    assert_eq!(
        server.request_count(),
        1,
        "the inspection must not call the provider"
    );

    // And it left no mark on the conversation the next turn will send.
    let db = app.open_database().await.unwrap();
    let messages_after_query = MessageRepository::new(&db).count(&session).await.unwrap();

    client
        .send(ClientCommand::SubmitMessage {
            session_id: session.clone(),
            content: "second question".into(),
            attachments: vec![],
        })
        .await
        .unwrap();
    wait_for_turn_end(&mut rx).await;

    let bodies = server.request_bodies().await;
    assert_eq!(bodies.len(), 2, "exactly one model request per turn");
    for body in &bodies {
        for marker in [
            "/context",
            "Context Inspector",
            "Breakdown",
            "Compact at",
            "Last compact",
            "●",
            "○",
        ] {
            assert!(
                !body.contains(marker),
                "inspection output leaked into the provider request: {marker:?}\n{body}"
            );
        }
    }
    // The second request is the conversation the user actually had.
    assert!(bodies[1].contains("first question"));
    assert!(bodies[1].contains("second question"));

    let messages_after_turn = MessageRepository::new(&db).count(&session).await.unwrap();
    assert_eq!(
        messages_after_query + 2,
        messages_after_turn,
        "the second turn adds its user + assistant message; the query added none"
    );
}

/// A timed-out summary consumes the remaining wall time even though no epoch
/// is committed. A second manual request must not receive that time again.
#[tokio::test]
async fn manual_compact_failure_consumes_duration_before_another_request() {
    let response = MockResponse::SilentThenJson {
        silent_ms: 2_000,
        body: serde_json::json!({
            "id":"slow-summary","choices":[{"message":{"role":"assistant","content":"summary"},"finish_reason":"stop"}],
            "usage":{"prompt_tokens":20,"completion_tokens":3}
        }).to_string(),
    };
    let (tmp, server, app, client, session) = harness(vec![response]).await;
    std::fs::create_dir_all(tmp.path().join(".leveler")).unwrap();
    std::fs::write(
        tmp.path().join(".leveler/config.yaml"),
        "limits:\n  max_duration_seconds: 1\n",
    )
    .unwrap();
    let db = app.open_database().await.unwrap();
    let payload = serde_json::json!({
        "version":2,
        "initiating_message":leveler_model::Message::text(leveler_model::Role::User,"task"),
        "objective":leveler_lifecycle::ObjectiveAnchor::from_user_message("task"),
        "continuation_root_turn_id":null,"goal_id":null
    })
    .to_string();
    let turn = leveler_storage::TurnStore::start(
        &db,
        &session,
        "chat",
        Some(&payload),
        leveler_core::now(),
    )
    .await
    .unwrap();
    let event = leveler_engine::EngineEvent::ProgressUpdated {
        ledger: leveler_lifecycle::ProgressLedger {
            budget_scope: Some(turn.id.clone()),
            cumulative_duration_ms: 500,
            ..Default::default()
        },
    };
    let (tag, payload) = event.to_row().unwrap();
    leveler_storage::EventRepository::new(&db)
        .append(&session, None, &tag, &payload, leveler_core::now())
        .await
        .unwrap();
    let history: Vec<_> = (0..4)
        .map(|n| {
            serde_json::to_string(&leveler_model::Message::text(
                leveler_model::Role::User,
                format!("original {n}"),
            ))
            .unwrap()
        })
        .collect();
    MessageRepository::new(&db)
        .append(&session, &history, leveler_core::now())
        .await
        .unwrap();
    let mut rx = client.subscribe();
    client
        .send(ClientCommand::CompactContext {
            session_id: session.clone(),
        })
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(10), async {
        while !matches!(rx.recv().await.unwrap(), RuntimeEvent::TurnFailed { .. }) {}
    })
    .await
    .unwrap();
    let progress = leveler_agent::load_auxiliary_budget_progress(&db, &db, &db, &session)
        .await
        .unwrap();
    assert_eq!(progress.budget_scope.as_deref(), Some(turn.id.as_str()));
    assert!(
        progress.cumulative_duration_ms >= 1000,
        "failed call must consume the half-second residual: {}",
        progress.cumulative_duration_ms
    );
    assert_eq!(server.request_count(), 1);
    tokio::task::yield_now().await;
    client
        .send(ClientCommand::CompactContext {
            session_id: session.clone(),
        })
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            if let RuntimeEvent::TurnFailed { error, .. } = rx.recv().await.unwrap() {
                assert!(
                    error.contains("预算"),
                    "second call must fail admission: {error}"
                );
                break;
            }
        }
    })
    .await
    .unwrap();
    assert_eq!(
        server.request_count(),
        1,
        "the second summary must not reach the provider"
    );
    let stored = MessageRepository::new(&db).load(&session).await.unwrap();
    let decode = |rows: Vec<String>| {
        rows.into_iter()
            .map(|row| serde_json::from_str::<serde_json::Value>(&row).unwrap())
            .collect::<Vec<_>>()
    };
    assert_eq!(decode(stored), decode(history));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn btw_uses_model_window_and_preserves_main_history() {
    use leveler_model::{Message, Role};
    let response = MockResponse::SilentThenJson { silent_ms: 0, body: serde_json::json!({
        "choices": [{"message": {"role": "assistant", "content": "answer"}, "finish_reason": "stop"}]
    }).to_string() };
    let (_tmp, server, app, client, session) =
        harness_with_window(vec![response], 128000, 100000).await;
    let db = app.open_database().await.unwrap();
    let repo = MessageRepository::new(&db);
    for i in 0..30 {
        repo.append(
            &session,
            &[serde_json::to_string(&Message::text(
                Role::User,
                format!("middle-marker-{i} {}", "abcdef ".repeat(1000)),
            ))
            .unwrap()],
            leveler_core::now(),
        )
        .await
        .unwrap();
    }
    let before = repo.load(&session).await.unwrap();
    let mut rx = client.subscribe();
    client
        .send(ClientCommand::Btw {
            session_id: session.clone(),
            question: "where is middle-marker-4?".into(),
        })
        .await
        .unwrap();
    loop {
        match tokio::time::timeout(Duration::from_secs(10), rx.recv())
            .await
            .unwrap()
            .unwrap()
        {
            RuntimeEvent::BtwCompleted => break,
            RuntimeEvent::BtwFailed { error } => panic!("{error}"),
            _ => {}
        }
    }
    let bodies = server.request_bodies().await;
    assert_eq!(bodies.len(), 1, "this profile has room without a summary");
    assert!(
        bodies[0].contains("middle-marker-8"),
        "the resolved model window must preserve middle history"
    );
    assert!(!bodies[0].contains("LOST"));
    assert_eq!(repo.load(&session).await.unwrap(), before);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn btw_accounts_for_side_history_and_only_folds_with_an_accepted_summary() {
    use leveler_model::{Message, Role};
    fn json_response(content: &str, finish: &str) -> MockResponse {
        MockResponse::SilentThenJson { silent_ms: 0, body: serde_json::json!({
            "choices": [{"message": {"role": "assistant", "content": content}, "finish_reason": finish}]
        }).to_string() }
    }
    async fn wait_btw(rx: &mut tokio::sync::broadcast::Receiver<RuntimeEvent>) -> bool {
        loop {
            match tokio::time::timeout(Duration::from_secs(10), rx.recv())
                .await
                .unwrap()
                .unwrap()
            {
                RuntimeEvent::BtwCompleted => return true,
                RuntimeEvent::BtwFailed { .. } => return false,
                _ => {}
            }
        }
    }
    for accepted in [true, false] {
        let side_answer = format!("side-history-unique {}", "abcdef ".repeat(1500));
        let mut responses = vec![
            json_response(&side_answer, "stop"),
            json_response("briefing", if accepted { "stop" } else { "length" }),
        ];
        if accepted {
            responses.push(json_response("final side answer", "stop"));
        }
        // The declared window has to leave room for the side surface's own
        // fixed cost (the mode contract plus the read-only tool schemas) on top
        // of the retained tail; otherwise no fold could ever bring the request
        // under budget and the truth would be a refusal, not a briefing.
        let (_tmp, server, app, client, session) =
            harness_with_window(responses, 32_768, 8_192).await;
        let db = app.open_database().await.unwrap();
        let repo = MessageRepository::new(&db);
        let history: Vec<String> = (0..10)
            .map(|i| {
                serde_json::to_string(&Message::text(
                    Role::User,
                    format!("main-{i} {}", "abcdef ".repeat(200)),
                ))
                .unwrap()
            })
            .collect();
        repo.append(&session, &history, leveler_core::now())
            .await
            .unwrap();
        let before = repo.load(&session).await.unwrap();
        let mut rx = client.subscribe();
        client
            .send(ClientCommand::Btw {
                session_id: session.clone(),
                question: "first side question".into(),
            })
            .await
            .unwrap();
        assert!(wait_btw(&mut rx).await);
        client
            .send(ClientCommand::Btw {
                session_id: session.clone(),
                question: "latest-side-question".into(),
            })
            .await
            .unwrap();
        assert_eq!(wait_btw(&mut rx).await, accepted);
        let bodies = server.request_bodies().await;
        assert_eq!(bodies.len(), if accepted { 3 } else { 2 });
        // The side conversation is what pushed the second question over
        // budget: the first one fitted without a fold, so exactly one extra
        // request (the briefing) appears here. The newest messages are the
        // retained tail, so what the briefing replaces is the older main
        // history, and the side turn itself stays verbatim in the folded
        // context rather than being paraphrased.
        assert!(
            bodies[1].contains("main-0") && !bodies[1].contains("side-history-unique"),
            "the briefing summarizes the older main history, not the newest turn"
        );
        if accepted {
            assert!(bodies[2].contains("briefing"));
            assert!(bodies[2].contains("latest-side-question"));
            assert!(
                bodies[2].contains("side-history-unique"),
                "the side turn stays verbatim in the folded context"
            );
            assert!(!bodies[2].contains("LOST"));
            let body: serde_json::Value = serde_json::from_str(&bodies[2]).unwrap();
            assert_eq!(body["max_tokens"], 1024);
        }
        assert_eq!(
            repo.load(&session).await.unwrap(),
            before,
            "side compaction cannot rewrite main history"
        );
        let calls = leveler_storage::ModelRequestRepository::new(&db)
            .load_for_session(&session)
            .await
            .unwrap();
        assert_eq!(
            calls
                .iter()
                .filter(|r| r.kind == leveler_storage::ModelCallKind::Compaction)
                .count(),
            1
        );
        if accepted {
            let final_call = calls
                .iter()
                .filter(|r| r.kind == leveler_storage::ModelCallKind::SideQuestion)
                .next_back()
                .unwrap();
            assert!(
                final_call
                    .estimated_tokens
                    .is_some_and(|tokens| tokens <= 8_192),
                "the exact final side projection must fit its resolved input threshold"
            );
        }
    }
}

/// The `/btw` side surface exposes real read-only tools and no mutating tool
/// or harness control. `find_references` and friends come from the same
/// capability composition a normal turn uses, narrowed to the observe class.
#[tokio::test]
async fn btw_side_surface_is_read_only_and_excludes_harness_controls() {
    let (_tmp, _server, app, _client, session) = harness(vec![]).await;
    let (registry, tool_context) = app
        .side_question_tools(
            &ModelRef::new("mock", "m"),
            PermissionProfile::Assisted,
            false,
            leveler_agent::WorkProfile::Balanced,
            Some(session.as_str()),
        )
        .await
        .unwrap();
    let names: Vec<String> = registry
        .definitions()
        .into_iter()
        .map(|definition| definition.name)
        .collect();
    for present in [
        "read_file",
        "grep",
        "list_files",
        "find_files",
        "read_project_rules",
        "find_references",
    ] {
        assert!(
            names.iter().any(|name| name == present),
            "a side question must be able to investigate with {present}: {names:?}"
        );
    }
    for forbidden in [
        "apply_patch",
        "write_file",
        "run_command",
        "shell_command",
        "update_plan",
        "update_goal",
        "spawn_agent",
        "request_permissions",
        "remember",
        "forget",
        "kill_task",
        "wait_task",
    ] {
        assert!(
            !names.iter().any(|name| name == forbidden),
            "a side question must not expose {forbidden}: {names:?}"
        );
    }
    // A mutating call has no route on the side surface, so it cannot reach the
    // workspace even if the model asks: the refusal is deterministic, not a
    // prompt the model is trusted to obey.
    let error = registry
        .execute(
            "write_file",
            serde_json::json!({"path": "created-by-btw.txt", "content": "x"}),
            tool_context,
            tokio_util::sync::CancellationToken::new(),
        )
        .await
        .expect_err("a mutating tool is not on the side surface");
    assert!(error.to_string().contains("unknown tool"), "{error}");
}

/// A side question may read the workspace to answer, but it cannot write the
/// main transcript, start a turn, or leak itself into the next main request.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn btw_can_call_a_read_only_tool_without_touching_the_main_task() {
    use leveler_storage::{GoalStore, TurnRepository};

    let tool_call = MockResponse::SilentThenJson {
        silent_ms: 0,
        body: serde_json::json!({
            "choices": [{
                "message": {"role": "assistant", "content": null, "tool_calls": [{
                    "id": "call_probe",
                    "type": "function",
                    "function": {
                        "name": "read_file",
                        "arguments": serde_json::json!({"path": "probe.txt"}).to_string()
                    }
                }]},
                "finish_reason": "tool_calls"
            }]
        })
        .to_string(),
    };
    let side_answer = MockResponse::SilentThenJson {
        silent_ms: 0,
        body: serde_json::json!({
            "choices": [{"message": {"role": "assistant", "content": "probe.txt says hello-from-probe"}, "finish_reason": "stop"}]
        })
        .to_string(),
    };
    let (tmp, server, app, client, session) = harness_with_window(
        vec![tool_call, side_answer, text("main answer")],
        128_000,
        100_000,
    )
    .await;
    std::fs::write(tmp.path().join("probe.txt"), "hello-from-probe\n").unwrap();

    let db = app.open_database().await.unwrap();
    let repo = MessageRepository::new(&db);
    let before_messages = repo.load(&session).await.unwrap();
    let before_goals = GoalStore::unfinished(&db).await.unwrap().len();
    let before_turns = TurnRepository::new(&db).list(&session).await.unwrap().len();

    let mut rx = client.subscribe();
    client
        .send(ClientCommand::Btw {
            session_id: session.clone(),
            question: "read probe.txt and tell me what it says".into(),
        })
        .await
        .unwrap();
    let mut answer = String::new();
    loop {
        match tokio::time::timeout(Duration::from_secs(15), rx.recv())
            .await
            .unwrap()
            .unwrap()
        {
            RuntimeEvent::BtwTextDelta { delta } => answer.push_str(&delta),
            RuntimeEvent::BtwCompleted => break,
            RuntimeEvent::BtwFailed { error } => panic!("{error}"),
            _ => {}
        }
    }
    assert_eq!(answer, "probe.txt says hello-from-probe");

    let bodies = server.request_bodies().await;
    assert_eq!(
        bodies.len(),
        2,
        "one read-only tool round: call then answer"
    );
    assert!(
        bodies[1].contains("hello-from-probe"),
        "the read-only tool result must reach the next side request: {}",
        bodies[1]
    );
    assert_eq!(
        repo.load(&session).await.unwrap(),
        before_messages,
        "a side question cannot write the main transcript"
    );
    assert_eq!(
        GoalStore::unfinished(&db).await.unwrap().len(),
        before_goals,
        "a side question cannot open a main goal"
    );
    assert_eq!(
        TurnRepository::new(&db).list(&session).await.unwrap().len(),
        before_turns,
        "a side question cannot start a main turn"
    );

    // The main task still works after the side question, and the side question
    // is not resurrected into its context.
    client
        .send(ClientCommand::SubmitMessage {
            session_id: session.clone(),
            content: "main question".into(),
            attachments: vec![],
        })
        .await
        .unwrap();
    wait_for_turn_end(&mut rx).await;
    let bodies = server.request_bodies().await;
    assert_eq!(
        bodies.len(),
        3,
        "exactly one main request after the side thread"
    );
    assert!(bodies[2].contains("main question"));
    assert!(
        !bodies[2].contains("read probe.txt and tell me what it says"),
        "the side question must not become main-task context"
    );
    assert_eq!(
        TurnRepository::new(&db).list(&session).await.unwrap().len(),
        before_turns + 1,
        "only the explicit main turn creates a turn"
    );
}

/// PR4 surface budget for `/btw`, measured from the request the provider
/// actually received.
///
/// A side question runs on its own fixed surface: the mode contract it is
/// handed plus the read-only tool descriptions and schemas. This is the `/btw`
/// half of the prompt-surface pin — `crates/leveler-agent/tests/
/// prompt_surface_budget.rs` pins the turn surface, and `/btw` is composed by
/// this crate ([`Application::side_question_tools`]), so its budget is read
/// here rather than in a second counter over there.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn btw_read_only_surface_stays_within_its_budget() {
    use leveler_model::{TokenEstimate, estimate_text};

    let response = MockResponse::SilentThenJson {
        silent_ms: 0,
        body: serde_json::json!({
            "choices": [{"message": {"role": "assistant", "content": "answer"}, "finish_reason": "stop"}]
        })
        .to_string(),
    };
    let (_tmp, server, _app, client, session) = harness(vec![response]).await;
    let mut rx = client.subscribe();
    client
        .send(ClientCommand::Btw {
            session_id: session.clone(),
            question: "为什么这样设计？".into(),
        })
        .await
        .unwrap();
    loop {
        match tokio::time::timeout(Duration::from_secs(15), rx.recv())
            .await
            .unwrap()
            .unwrap()
        {
            RuntimeEvent::BtwCompleted => break,
            RuntimeEvent::BtwFailed { error } => panic!("{error}"),
            _ => {}
        }
    }

    let bodies = server.request_bodies().await;
    assert_eq!(bodies.len(), 1, "one side request without a tool call");
    let body: serde_json::Value = serde_json::from_str(&bodies[0]).unwrap();
    // Read the wire spelling, so the pin measures what the provider receives.
    let tools = body["tools"]
        .as_array()
        .expect("the side request carries tools");

    // The mode contract, as the model reads it, is one user message.
    let contract = body["messages"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|message| message["content"].as_str())
        .filter(|content| content.contains("旁问"))
        .map(estimate_text)
        .sum::<u64>();
    assert!(contract > 0, "the side request carries the /btw contract");

    let mut description = TokenEstimate::new();
    let mut schema = TokenEstimate::new();
    for tool in tools {
        let function = &tool["function"];
        description.add_text(function["description"].as_str().unwrap_or_default());
        schema.add_tool(function["name"].as_str().unwrap_or_default());
        schema.add_tool(&function["parameters"].to_string());
    }
    let description = description.tokens();
    let schema = schema.tokens();
    let tool_tokens = description + schema;
    let fixed = contract + tool_tokens;

    let names: Vec<&str> = tools
        .iter()
        .map(|tool| tool["function"]["name"].as_str().unwrap_or_default())
        .collect();
    for forbidden in [
        "apply_patch",
        "write_file",
        "edit_file",
        "run_command",
        "shell_command",
        "update_plan",
        "update_goal",
        "spawn_agent",
        "request_permissions",
        "request_user_input",
        "remember",
        "forget",
        "kill_task",
        "wait_task",
        "create_checkpoint",
    ] {
        assert!(
            !names.contains(&forbidden),
            "a side question must not advertise {forbidden}: {names:?}"
        );
    }

    eprintln!(
        "SIDE_QUESTION_SURFACE contract {contract} description {description} \
         schema {schema} tools {tool_tokens} fixed {fixed} tool_count {}\n  tools {names:?}",
        tools.len()
    );
    // Same pin shape as the turn surface: 5% and at least 500 tokens of slack,
    // so a description copy edit does not fail the gate and a new tool does.
    // Measured in this repository with the host capabilities this test fixture
    // resolves (code intelligence, git, project rules, skills, memory; no
    // search key and no browser handle).
    const BASELINE: u64 = 2_910;
    let slack = (BASELINE / 20).max(500);
    assert!(
        fixed <= BASELINE + slack,
        "side-question fixed surface {fixed} exceeds baseline {BASELINE} + {slack}"
    );
}

/// A tool-bearing `/btw` request must be projected as the request that is
/// actually sent.
///
/// The side question carries the read-only tool surface, so a route whose
/// contract replays captured reasoning on tool-bearing requests must replay the
/// reasoning of the side conversation's own tool turns. The projection sent on
/// the request — and the pressure figure the runtime records for it — is
/// computed from the same tool list the request carries; a projection built as
/// if no tools existed would drop the reasoning the provider validates and
/// would understate the request's size.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn btw_projects_the_request_it_sends_including_its_tools() {
    let thinking = "先读取探针文件，再回答。";
    let tool_call = MockResponse::SilentThenJson {
        silent_ms: 0,
        body: serde_json::json!({
            "choices": [{
                "message": {
                    "role": "assistant",
                    "content": null,
                    "reasoning_content": thinking,
                    "tool_calls": [{
                        "id": "call_probe",
                        "type": "function",
                        "function": {
                            "name": "read_file",
                            "arguments": serde_json::json!({"path": "probe.txt"}).to_string()
                        }
                    }]
                },
                "finish_reason": "tool_calls"
            }]
        })
        .to_string(),
    };
    let answer = MockResponse::SilentThenJson {
        silent_ms: 0,
        body: serde_json::json!({
            "choices": [{"message": {"role": "assistant", "content": "probe.txt says hello-from-probe"}, "finish_reason": "stop"}]
        })
        .to_string(),
    };
    let (_tmp, server, _app, client, session) = harness_with_model(
        vec![tool_call, answer],
        131_072,
        65_536,
        "capabilities: { streaming: true, tool_calling: true, parallel_tool_calls: false, \
         structured_output: true, reasoning: true, vision: false }\nreasoning: { style: thinking_flag, \
         supported_efforts: [low, high, max], default_effort: max }",
        "compatibility: { synthesize_tool_call_ids: true, drop_unsupported_fields: true, \
         reasoning_replay_scope: when_tools_present, reasoning_content_key_required: true }",
    )
    .await;
    let tmp = tempfile::tempdir().unwrap();
    std::fs::write(tmp.path().join("probe.txt"), "hello-from-probe\n").unwrap();

    let mut rx = client.subscribe();
    client
        .send(ClientCommand::Btw {
            session_id: session.clone(),
            question: "read probe.txt and tell me what it says".into(),
        })
        .await
        .unwrap();
    loop {
        match tokio::time::timeout(Duration::from_secs(15), rx.recv())
            .await
            .unwrap()
            .unwrap()
        {
            RuntimeEvent::BtwCompleted => break,
            RuntimeEvent::BtwFailed { error } => panic!("{error}"),
            _ => {}
        }
    }

    let bodies = server.request_bodies().await;
    assert_eq!(bodies.len(), 2, "one read-only tool round");
    let second: serde_json::Value = serde_json::from_str(&bodies[1]).unwrap();
    assert!(
        !second["tools"].as_array().unwrap().is_empty(),
        "the second side request still carries the read-only tools"
    );
    let replayed: Vec<&str> = second["messages"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|m| m["role"] == "assistant")
        .filter_map(|m| m["reasoning_content"].as_str())
        .collect();
    assert!(
        replayed.contains(&thinking),
        "a tool-bearing side request must replay its tool turn's reasoning: {replayed:?}"
    );
}

/// When the declared budget cannot hold the side surface's own fixed cost plus
/// a minimal retained tail, the side question reports that truth and stops. It
/// does not send the over-budget request, and it does not touch the main
/// history or write a snapshot for a fold that never happened.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn btw_reports_an_unreachable_budget_instead_of_sending_it() {
    use leveler_model::{Message, Role};
    let response = MockResponse::SilentThenJson {
        silent_ms: 0,
        body: serde_json::json!({
            "choices": [{"message": {"role": "assistant", "content": "briefing"}, "finish_reason": "stop"}]
        })
        .to_string(),
    };
    // 4 096 reliable tokens against a read-only surface of ~2.9k: after a fold
    // the fixed cost plus any retained tail still exceeds it.
    let (_tmp, server, app, client, session) = harness(vec![response]).await;
    let db = app.open_database().await.unwrap();
    let repo = MessageRepository::new(&db);
    let history: Vec<String> = (0..8)
        .map(|i| {
            serde_json::to_string(&Message::text(
                Role::User,
                format!("main-{i} {}", "abcdef ".repeat(200)),
            ))
            .unwrap()
        })
        .collect();
    repo.append(&session, &history, leveler_core::now())
        .await
        .unwrap();
    let before = repo.load(&session).await.unwrap();
    let mut rx = client.subscribe();
    client
        .send(ClientCommand::Btw {
            session_id: session.clone(),
            question: "answer anyway".into(),
        })
        .await
        .unwrap();
    let failed = loop {
        match tokio::time::timeout(Duration::from_secs(15), rx.recv())
            .await
            .unwrap()
            .unwrap()
        {
            RuntimeEvent::BtwFailed { error } => break error,
            RuntimeEvent::BtwCompleted => {
                panic!("an unfittable side request must not report success")
            }
            _ => {}
        }
    };
    assert!(
        failed.contains("超过上下文预算") || failed.contains("超过模型窗口"),
        "the failure names the budget: {failed}"
    );
    assert_eq!(
        repo.load(&session).await.unwrap(),
        before,
        "a refused fold cannot rewrite the main transcript"
    );
    let snapshots = leveler_storage::EventRepository::new(&db)
        .load_after(&session, 0)
        .await
        .unwrap()
        .into_iter()
        .filter(|event| event.event_type == "context_snapshot")
        .count();
    assert_eq!(snapshots, 0, "a refused fold writes no snapshot");
    // Only the summarization attempts reached the provider; the over-budget
    // side request itself was never sent.
    let bodies = server.request_bodies().await;
    assert!(
        bodies.iter().all(|body| !body.contains("answer anyway")),
        "the over-budget side request must not be sent: {} bodies",
        bodies.len()
    );
}
