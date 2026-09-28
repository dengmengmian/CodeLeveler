//! Independent budget-lineage checks against real SQLite and the public chat API.
use async_trait::async_trait;
use leveler_agent::coding::{CodingRuntime, ExecutorFactory, TaskSpec};
use leveler_agent::{AutoClarify, StepLimits};
use leveler_core::{RequestId, SessionId, TurnId};
use leveler_engine::{EngineEvent, ExecutionKind, TaskEngine};
use leveler_execution::{AutoApprove, PermissionProfile, Workspace};
use leveler_model::{
    FinishReason, Message, ModelError, ModelErrorKind, ModelEvent, ModelEventStream, ModelProfile,
    ModelRef, ModelRequest, ModelResponse, ModelRuntime, Role, TokenUsage,
};
use leveler_storage::{
    Database, EventStore, MessageRepository, ModelRequestRepository, SessionRecord,
    SessionRepository, TurnRepository,
};
use leveler_tools::ToolContext;
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, Ordering},
};
use tokio_util::sync::CancellationToken;

struct Runtime {
    db: Database,
    session: SessionId,
    fail_summary: AtomicBool,
    retry_summary: AtomicBool,
    partial_main: AtomicBool,
    // Whether each call is a summary, and the durable running turn seen before sending.
    calls: Mutex<Vec<(bool, Option<String>)>>,
}
#[async_trait]
impl ModelRuntime for Runtime {
    async fn profile(&self, _: &ModelRef) -> Result<ModelProfile, ModelError> {
        Ok(serde_json::from_value(serde_json::json!({
            "id":"m","provider":"mock","model_id":"m","protocol":"openai_chat",
            "capabilities":{"streaming":true,"tool_calling":true,"parallel_tool_calls":true,"structured_output":false,"reasoning":false,"vision":false},
            "limits":{"context_window":64000,"reliable_context":32000,"max_output_tokens":2048,"max_tool_schema_bytes":65536,"max_parallel_tool_calls":4},
            "pricing":{"input_usd_per_mtok":1.0,"output_usd_per_mtok":4.0}
        })).unwrap())
    }
    async fn generate(
        &self,
        _: ModelRequest,
        _: CancellationToken,
    ) -> Result<ModelResponse, ModelError> {
        unreachable!("tracked chat calls stream")
    }
    async fn stream(
        &self,
        request: ModelRequest,
        _: CancellationToken,
    ) -> Result<ModelEventStream, ModelError> {
        let summary = request.tools.is_empty();
        let turns = TurnRepository::new(&self.db)
            .list_running(Some(&self.session))
            .await
            .unwrap();
        self.calls
            .lock()
            .unwrap()
            .push((summary, turns.last().map(|turn| turn.id.clone())));
        let usage = TokenUsage {
            input_tokens: 30,
            output_tokens: 5,
            ..Default::default()
        };
        if !summary && self.partial_main.load(Ordering::SeqCst) {
            return Ok(Box::pin(futures::stream::iter(vec![
                Ok(ModelEvent::UsageUpdated { usage }),
                Ok(ModelEvent::ReasoningDelta {
                    delta: "private unfinished reasoning".into(),
                }),
                Ok(ModelEvent::TextDelta {
                    delta: "Observed but incomplete answer.".into(),
                }),
                Ok(ModelEvent::ToolCallStarted {
                    index: 0,
                    id: Some(leveler_core::ToolCallId::new("partial")),
                    name: Some("run_command".into()),
                }),
                Ok(ModelEvent::ToolCallArgumentsDelta {
                    index: 0,
                    delta: "{\"command\":\"do not execute".into(),
                }),
                Ok(ModelEvent::Error {
                    error: ModelError::new(ModelErrorKind::StreamInterrupted, "connection lost"),
                }),
            ])));
        }
        if summary && self.retry_summary.swap(false, Ordering::SeqCst) {
            return Ok(Box::pin(futures::stream::iter(vec![
                Ok(ModelEvent::UsageUpdated {
                    usage: TokenUsage {
                        input_tokens: 490_000,
                        output_tokens: 0,
                        ..Default::default()
                    },
                }),
                Ok(ModelEvent::Error {
                    error: ModelError::new(ModelErrorKind::Transport, "temporary disconnect")
                        .with_retry_after_ms(0),
                }),
            ])));
        }
        if summary && self.fail_summary.load(Ordering::SeqCst) {
            // The assertion below checks elapsed spend, so this delay is the behavior under test.
            tokio::time::sleep(std::time::Duration::from_millis(25)).await;
            return Ok(Box::pin(futures::stream::iter(vec![
                Ok(ModelEvent::UsageUpdated { usage }),
                Ok(ModelEvent::ReasoningDelta {
                    delta: "paid partial summary".into(),
                }),
                Ok(ModelEvent::Error {
                    error: ModelError::new(ModelErrorKind::Decode, "invalid summary stream"),
                }),
            ])));
        }
        Ok(leveler_model::stream_from_response(ModelResponse {
            request_id: RequestId::generate(),
            message: Message::text(
                Role::Assistant,
                if summary {
                    "Prior work recap."
                } else {
                    "Answer."
                },
            ),
            finish_reason: FinishReason::Stop,
            usage,
        }))
    }
}
struct Fixture {
    runtime: Arc<Runtime>,
    coding: CodingRuntime,
    db: Database,
    session: SessionId,
    dir: tempfile::TempDir,
}
async fn fixture() -> Fixture {
    let dir = tempfile::tempdir().unwrap();
    let db = Database::connect_in_memory().await.unwrap();
    let record = SessionRecord::new(
        dir.path().to_string_lossy().as_ref(),
        "chat",
        "mock/m",
        leveler_core::now(),
    );
    SessionRepository::new(&db).create(&record).await.unwrap();
    let session = SessionId::new(record.id);
    let runtime = Arc::new(Runtime {
        db: db.clone(),
        session: session.clone(),
        fail_summary: AtomicBool::new(false),
        retry_summary: AtomicBool::new(false),
        partial_main: AtomicBool::new(false),
        calls: Mutex::new(vec![]),
    });
    let context = ToolContext::with_environment(
        Workspace::new(dir.path()).unwrap(),
        PermissionProfile::Assisted,
        Arc::new(leveler_core::EnvSnapshot::new(
            std::env::vars_os(),
            dir.path().into(),
            std::env::temp_dir(),
        )),
    );
    let mut registry = leveler_tools::default_registry();
    leveler_agent::register_harness_controls(&mut registry);
    let coding = CodingRuntime {
        engine: TaskEngine {
            stores: leveler_storage::EngineStores::from_database(&db),
            runtime_id: leveler_core::RuntimeId::new("budget-review"),
            boot: leveler_engine::EngineBoot {
                id: leveler_core::BootId::generate(),
                liveness: Arc::new(leveler_test_support::TestBoots::new()),
            },
        },
        factory: ExecutorFactory {
            runtime: runtime.clone(),
            registry: Arc::new(registry),
            tool_context: context,
            model: ModelRef::new("mock", "m"),
            commit_co_author: true,
            overrides: None,
            memory_catalog: String::new(),
            memory_expose: true,
            memory_root: None,
            background_tasks: Arc::new(leveler_execution::BackgroundTaskRegistry::new()),
            permission_rules: Default::default(),
            permission_rules_path: None,
            hook_runner: leveler_execution::HookRunner::empty(".".into()),
            steering: None,
            allow_delegation: true,
            independent_review: leveler_agent::coding::IndependentReviewPolicy::Off,
            develop_model: None,
        },
        approver: Arc::new(AutoApprove),
        clarifier: Arc::new(AutoClarify),
        task_cancel: Arc::new(AtomicBool::new(false)),
    };
    Fixture {
        runtime,
        coding,
        db,
        session,
        dir,
    }
}
fn spec(fx: &Fixture) -> TaskSpec {
    TaskSpec {
        runtime: leveler_agent::coding::RuntimeTaskSpec {
            goal: "chat".into(),
            kind: ExecutionKind::Direct,
            continuation: leveler_agent::ContinuationPolicy::UntilTerminal,
            limits: StepLimits {
                max_model_tokens: Some(500_000),
                max_cost_usd_micros: Some(1_000_000),
                max_duration: Some(std::time::Duration::from_secs(30)),
                ..Default::default()
            },
        },
        coding: leveler_agent::coding::CodingTaskSpec {
            repository: fx.dir.path().into(),
            mode: PermissionProfile::Assisted,
            sandbox: false,
        },
    }
}
async fn add_long_history(fx: &Fixture) {
    let messages: Vec<_> = (0..80)
        .map(|index| {
            serde_json::to_string(&Message::text(
                if index % 2 == 0 {
                    Role::User
                } else {
                    Role::Assistant
                },
                "history ".repeat(2000),
            ))
            .unwrap()
        })
        .collect();
    MessageRepository::new(&fx.db)
        .append(&fx.session, &messages, leveler_core::now())
        .await
        .unwrap();
}
async fn chat(
    fx: &Fixture,
) -> Result<leveler_agent::coding::TaskReport, leveler_engine::EngineError> {
    fx.coding
        .chat(
            &fx.session,
            &spec(fx),
            vec![leveler_model::ContentPart::Text {
                text: "new question".into(),
            }],
            &mut |_| {},
            CancellationToken::new(),
        )
        .await
}
#[tokio::test]
async fn new_chat_summary_has_its_own_durable_budget_before_sending() {
    let fx = fixture().await;
    chat(&fx).await.unwrap();
    let first_turn = TurnRepository::new(&fx.db).list(&fx.session).await.unwrap()[0]
        .id
        .clone();
    let old = leveler_lifecycle::ProgressLedger {
        budget_scope: Some(first_turn.clone()),
        cumulative_model_tokens: 9_000_000,
        cumulative_cost_usd_micros: 9_000_000,
        cumulative_duration_ms: 9_000_000,
        has_unpriced_model_attempt: true,
        ..Default::default()
    };
    let (kind, payload) = EngineEvent::ProgressUpdated { ledger: old }
        .to_row()
        .unwrap();
    EventStore::append(
        &fx.db,
        &fx.session,
        Some(&TurnId::new(first_turn.clone())),
        &kind,
        &payload,
        leveler_core::now(),
    )
    .await
    .unwrap();
    add_long_history(&fx).await;
    chat(&fx).await.unwrap();
    let summary_turn = {
        let calls = fx.runtime.calls.lock().unwrap();
        calls
            .iter()
            .find(|(summary, _)| *summary)
            .expect("old debt must not suppress the new chat summary")
            .1
            .as_ref()
            .expect("summary must see a durable running turn")
            .clone()
    };
    assert_ne!(summary_turn, first_turn);
    let records = ModelRequestRepository::new(&fx.db)
        .load_for_session(&fx.session)
        .await
        .unwrap();
    let summaries: Vec<_> = records
        .iter()
        .filter(|r| r.kind == leveler_storage::ModelCallKind::Compaction)
        .collect();
    assert!(!summaries.is_empty());
    assert!(
        summaries
            .iter()
            .all(|record| record.budget_scope.as_deref() == Some(summary_turn.as_str()))
    );
}
#[tokio::test]
async fn failed_paid_preparation_keeps_its_bill_and_terminal_turn() {
    let fx = fixture().await;
    add_long_history(&fx).await;
    fx.runtime.fail_summary.store(true, Ordering::SeqCst);
    assert!(chat(&fx).await.is_err());
    let turns = TurnRepository::new(&fx.db).list(&fx.session).await.unwrap();
    assert_eq!(
        turns.len(),
        1,
        "paid preparation must belong to a recoverable turn"
    );
    assert_eq!(turns[0].status, "failed");
    let records = ModelRequestRepository::new(&fx.db)
        .load_for_session(&fx.session)
        .await
        .unwrap();
    assert_eq!(
        records.len(),
        1,
        "reasoning failure must not be blindly retried"
    );
    assert_eq!(
        records[0].budget_scope.as_deref(),
        Some(turns[0].id.as_str())
    );
    assert_eq!(records[0].input_tokens + records[0].output_tokens, 35);
    assert!(records[0].error_kind.is_some());
    assert!(records[0].cost_usd_micros.is_some());
    let ledger = leveler_agent::load_auxiliary_budget_progress(&fx.db, &fx.db, &fx.db, &fx.session)
        .await
        .unwrap();
    assert!(
        ledger.cumulative_duration_ms >= 20,
        "the first summary's duration must survive initial scope assignment"
    );
}

#[tokio::test]
async fn request_insert_before_progress_event_is_recovered_in_the_current_turn_scope() {
    let fx = fixture().await;
    chat(&fx).await.unwrap();
    let old_turn = TurnRepository::new(&fx.db)
        .list(&fx.session)
        .await
        .unwrap()
        .remove(0);
    let current = TurnRepository::new(&fx.db)
        .start(
            &fx.session,
            "chat",
            old_turn.payload.as_deref(),
            leveler_core::now(),
        )
        .await
        .unwrap();
    let repo = ModelRequestRepository::new(&fx.db);
    let mut row = repo.load_for_session(&fx.session).await.unwrap().remove(0);
    row.id = "crash-child-attempt".into();
    row.budget_scope = Some(current.id.clone());
    row.input_tokens = 0;
    row.output_tokens = 0;
    row.cost_usd_micros = None;
    row.estimated_tokens = Some(37);
    row.finish_reason = None;
    row.error_kind = Some("transport".into());
    row.agent_id = Some("child".into());
    repo.insert(&row).await.unwrap();
    row.id = "known-attempt".into();
    row.input_tokens = 10;
    row.output_tokens = 5;
    row.cost_usd_micros = Some(70);
    row.estimated_tokens = None;
    repo.insert(&row).await.unwrap();
    // An unrelated, later side-question row must not choose the budget owner.
    row.id = "unrelated-late-attempt".into();
    row.budget_scope = Some(old_turn.id);
    row.input_tokens = 90_000;
    row.output_tokens = 10_000;
    row.cost_usd_micros = Some(90_000);
    repo.insert(&row).await.unwrap();
    for _ in 0..2 {
        let ledger =
            leveler_agent::load_auxiliary_budget_progress(&fx.db, &fx.db, &fx.db, &fx.session)
                .await
                .unwrap();
        assert_eq!(ledger.budget_scope.as_deref(), Some(current.id.as_str()));
        assert_eq!(ledger.cumulative_model_tokens, 52);
        assert_eq!(ledger.cumulative_estimated_model_tokens, 37);
        assert_eq!(ledger.cumulative_cost_usd_micros, 70);
        assert!(ledger.has_unpriced_model_attempt);
    }
}

#[tokio::test]
async fn legacy_spend_without_scope_refuses_a_capped_resume_before_calling_model() {
    let fx = fixture().await;
    SessionRepository::new(&fx.db)
        .set_execution(
            &fx.session,
            "assisted",
            false,
            "direct",
            leveler_core::now(),
        )
        .await
        .unwrap();
    SessionRepository::new(&fx.db)
        .set_outcome(
            &fx.session,
            leveler_engine::TaskOutcome::Interrupted,
            leveler_core::now(),
        )
        .await
        .unwrap();
    let initial = Message::text(Role::User, "legacy interrupted question");
    let payload = serde_json::json!({"version":1,"message":initial}).to_string();
    let turn = TurnRepository::new(&fx.db)
        .start(&fx.session, "chat", Some(&payload), leveler_core::now())
        .await
        .unwrap();
    TurnRepository::new(&fx.db)
        .finish(&TurnId::new(&turn.id), "interrupted", leveler_core::now())
        .await
        .unwrap();
    MessageRepository::new(&fx.db)
        .append_in_turn(
            &fx.session,
            &TurnId::new(&turn.id),
            &[serde_json::to_string(&initial).unwrap()],
            leveler_core::now(),
        )
        .await
        .unwrap();
    let legacy = leveler_lifecycle::ProgressLedger {
        cumulative_model_tokens: 100,
        cumulative_cost_usd_micros: 50,
        ..Default::default()
    };
    let (kind, payload) = EngineEvent::ProgressUpdated { ledger: legacy }
        .to_row()
        .unwrap();
    EventStore::append(
        &fx.db,
        &fx.session,
        Some(&TurnId::new(&turn.id)),
        &kind,
        &payload,
        leveler_core::now(),
    )
    .await
    .unwrap();
    let error = fx
        .coding
        .resume(
            &fx.session,
            &spec(&fx),
            &mut |_| {},
            CancellationToken::new(),
        )
        .await
        .unwrap_err();
    assert!(
        error.to_string().contains("legacy"),
        "expected explicit unreconstructable legacy budget: {error}"
    );
    assert!(
        fx.runtime.calls.lock().unwrap().is_empty(),
        "uncertain prior spend must not buy another model call"
    );
}

#[tokio::test]
async fn manual_loader_refuses_same_lineage_legacy_spend_even_after_a_scoped_row() {
    let fx = fixture().await;
    chat(&fx).await.unwrap();
    let repo = ModelRequestRepository::new(&fx.db);
    let row = repo.load_for_session(&fx.session).await.unwrap().remove(0);
    let legacy = TurnRepository::new(&fx.db)
        .start(&fx.session, "chat", None, leveler_core::now())
        .await
        .unwrap();
    let (kind, payload) = EngineEvent::ProgressUpdated {
        ledger: leveler_lifecycle::ProgressLedger {
            cumulative_model_tokens: 100,
            cumulative_cost_usd_micros: 50,
            ..Default::default()
        },
    }
    .to_row()
    .unwrap();
    EventStore::append(
        &fx.db,
        &fx.session,
        Some(&TurnId::new(&legacy.id)),
        &kind,
        &payload,
        leveler_core::now(),
    )
    .await
    .unwrap();
    let mut new_row = row;
    new_row.id = "first-scoped-after-legacy".into();
    new_row.budget_scope = Some(legacy.id.clone());
    repo.insert(&new_row).await.unwrap();
    let error = leveler_agent::load_auxiliary_budget_progress(&fx.db, &fx.db, &fx.db, &fx.session)
        .await
        .unwrap_err();
    assert!(
        error.to_string().contains("legacy"),
        "must expose unreconstructable prior spend: {error}"
    );

    let fresh = TurnRepository::new(&fx.db)
        .start(&fx.session, "chat", None, leveler_core::now())
        .await
        .unwrap();
    let ledger = leveler_agent::load_auxiliary_budget_progress(&fx.db, &fx.db, &fx.db, &fx.session)
        .await
        .unwrap();
    assert_eq!(ledger.budget_scope.as_deref(), Some(fresh.id.as_str()));
    assert_eq!(
        ledger.cumulative_model_tokens, 0,
        "another turn's legacy spend is unrelated"
    );
    assert_eq!(ledger.cumulative_cost_usd_micros, 0);
}

#[tokio::test]
async fn paid_summary_failure_cannot_retry_when_remaining_budget_cannot_fit_request() {
    let fx = fixture().await;
    add_long_history(&fx).await;
    fx.runtime.retry_summary.store(true, Ordering::SeqCst);
    let result = chat(&fx).await.unwrap();
    assert_eq!(
        result.stop_reason,
        leveler_agent::StopReason::BudgetExhausted,
        "the preserved transcript cannot fit the residual budget; stop must remain resumable"
    );
    assert_eq!(
        fx.runtime.calls.lock().unwrap().len(),
        1,
        "neither a summary retry nor the larger main request may exceed the residual budget"
    );
    let records = ModelRequestRepository::new(&fx.db)
        .load_for_session(&fx.session)
        .await
        .unwrap();
    assert_eq!(records.len(), 1);
    assert_eq!(records[0].input_tokens, 490_000);
}

#[tokio::test]
async fn observed_partial_body_survives_failure_as_incomplete_history_only() {
    let fx = fixture().await;
    fx.runtime.partial_main.store(true, Ordering::SeqCst);
    assert!(
        chat(&fx).await.is_err(),
        "partial text must not turn a failed call into an answer"
    );
    assert_eq!(
        fx.runtime.calls.lock().unwrap().len(),
        1,
        "partial output must not trigger blind retry"
    );
    let stored = MessageRepository::new(&fx.db)
        .load(&fx.session)
        .await
        .unwrap();
    let messages: Vec<Message> = stored
        .iter()
        .map(|row| serde_json::from_str(row).unwrap())
        .collect();
    let partial: Vec<_> = messages
        .iter()
        .filter(|message| message.role == Role::Assistant)
        .collect();
    assert_eq!(
        partial.len(),
        1,
        "observed partial answer must be durable exactly once"
    );
    assert!(
        stored.iter().any(
            |row| serde_json::from_str::<serde_json::Value>(row).unwrap()["incomplete"] == true
        ),
        "the raw row must carry host-owned incomplete metadata, independent of body text"
    );
    assert!(
        partial[0]
            .text_content()
            .starts_with("Observed but incomplete answer.")
    );
    assert!(partial[0].text_content().contains("interrupted"));
    assert!(
        partial[0]
            .content
            .iter()
            .all(|part| matches!(part, leveler_model::ContentPart::Text { .. })),
        "incomplete tools and reasoning must not become replayable content"
    );
    let turns = TurnRepository::new(&fx.db).list(&fx.session).await.unwrap();
    assert_eq!(turns[0].status, "failed");
}

struct ProfileOnly(ModelProfile);
#[async_trait]
impl ModelRuntime for ProfileOnly {
    async fn profile(&self, _: &ModelRef) -> Result<ModelProfile, ModelError> {
        Ok(self.0.clone())
    }
    async fn stream(
        &self,
        _: ModelRequest,
        _: CancellationToken,
    ) -> Result<ModelEventStream, ModelError> {
        unreachable!()
    }
    async fn generate(
        &self,
        _: ModelRequest,
        _: CancellationToken,
    ) -> Result<ModelResponse, ModelError> {
        unreachable!()
    }
}
#[tokio::test]
async fn summary_output_uses_resolved_capacity_and_leaves_room_after_fixed_thinking() {
    let fx = fixture().await;
    let model = ModelRef::new("mock", "m");
    let mut profile = fx.runtime.profile(&model).await.unwrap();
    profile.limits.max_output_tokens = 8192;
    profile.capabilities.reasoning = true;
    profile.reasoning.style = leveler_model::ReasoningStyle::BudgetedThinking {
        budget_tokens: 4096,
    };
    let runtime = ProfileOnly(profile);
    let messages: Vec<_> = (0..8)
        .map(|n| {
            Message::text(
                if n % 2 == 0 {
                    Role::User
                } else {
                    Role::Assistant
                },
                format!("context {n}"),
            )
        })
        .collect();
    let request = leveler_context::summary_request(&runtime, &model, None, &messages, 2, 0, 6000)
        .await
        .unwrap();
    assert_eq!(
        request.max_output_tokens,
        Some(6000),
        "resolved output capacity must fund both fixed thinking and the briefing"
    );
    let request = leveler_context::summary_request(&runtime, &model, None, &messages, 2, 0, 20000)
        .await
        .unwrap();
    assert_eq!(
        request.max_output_tokens,
        Some(8192),
        "summary must respect the provider limit"
    );
    assert!(
        leveler_context::summary_request(&runtime, &model, None, &messages, 2, 0, 4096)
            .await
            .is_none(),
        "no valid briefing budget remains after fixed thinking; retain history instead of sending an invalid request"
    );
}
