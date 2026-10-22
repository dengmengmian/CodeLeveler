//! Project memory policy and first-round exposure.
//!
//! `memory.enabled = false` is a project switch, not a permission level: no
//! profile may override it. When it IS enabled, the model must see the memory
//! tools and the guidance on its FIRST request, without spending a round on
//! `capability(enable)`.

use std::sync::Arc;

use leveler_app::Application;
use leveler_execution::PermissionProfile;
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

fn text_response(content: &str) -> MockResponse {
    let frame = serde_json::json!({
        "choices": [{"delta": {"content": content}, "finish_reason": "stop"}]
    });
    MockResponse::Sse {
        body: format!("data: {frame}\n\ndata: [DONE]\n\n"),
    }
}

fn stage_project(tmp: &tempfile::TempDir, base_url: &str, memory: Option<&str>) {
    std::fs::create_dir_all(tmp.path().join("configs/providers")).unwrap();
    std::fs::create_dir_all(tmp.path().join("configs/models")).unwrap();
    std::fs::write(
        tmp.path().join("configs/providers/mock.yaml"),
        format!("id: mock\nprotocol: openai_chat\nbase_url: {base_url}\n"),
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
    if let Some(value) = memory {
        std::fs::create_dir_all(tmp.path().join(".leveler")).unwrap();
        std::fs::write(
            tmp.path().join(".leveler/config.yaml"),
            format!("memory:\n  enabled: {value}\n"),
        )
        .unwrap();
    }
}

fn assemble(
    tmp: &tempfile::TempDir,
    collaboration: leveler_agent::CollaborationMode,
) -> Arc<Application> {
    let layout = Layout::from_parts(
        tmp.path().to_path_buf(),
        tmp.path().join("configs"),
        tmp.path().join("state"),
    );
    Arc::new(
        Application::assemble(layout)
            .unwrap()
            .with_collaboration(collaboration),
    )
}

async fn harness(memory: Option<&str>) -> (tempfile::TempDir, MockServer, Arc<Application>) {
    harness_with(memory, leveler_agent::CollaborationMode::Chat).await
}

async fn harness_with(
    memory: Option<&str>,
    collaboration: leveler_agent::CollaborationMode,
) -> (tempfile::TempDir, MockServer, Arc<Application>) {
    isolate_global_config();
    let server = MockServer::start(vec![text_response("done")]).await;
    let tmp = tempfile::tempdir().unwrap();
    stage_project(&tmp, &server.base_url(), memory);
    let app = assemble(&tmp, collaboration);
    (tmp, server, app)
}

async fn run_turn(
    app: &Application,
    session: &leveler_core::SessionId,
    mode: PermissionProfile,
    task: &str,
) {
    let model = ModelRef::new("mock", "m");
    app.run_in_session(
        session,
        &model,
        mode,
        task,
        Arc::new(leveler_execution::AutoApprove),
        false,
        &mut |_| {},
        tokio_util::sync::CancellationToken::new(),
    )
    .await
    .unwrap();
}

async fn run_chat(app: &Application, mode: PermissionProfile, task: &str) {
    let model = ModelRef::new("mock", "m");
    let session = app.create_session(&model, task).await.unwrap();
    run_turn(app, &session, mode, task).await;
}

async fn experience_rows(app: &Application) -> u64 {
    let db = app.open_database().await.unwrap();
    let counts = leveler_storage::ExperienceInboxRepository::new(&db)
        .counts()
        .await
        .unwrap();
    counts.pending + counts.processing + counts.failed_retryable + counts.processed
}

fn experience_candidate() -> leveler_memory::MemoryCandidate {
    let key = leveler_memory::semantic_key_of("cargo.test.dev_profile");
    let mut experience = leveler_memory::MemoryCandidate::new(
        "dev profile 缺少 test feature",
        "运行 cargo test 前先确认 dev profile 启用了 test feature。",
        leveler_memory::CandidateKind::Free,
        Some(key.clone()),
        leveler_memory::CandidateSource::RuntimeObserved,
        vec!["experience".to_string()],
    )
    .unwrap();
    experience.semantic_key = Some(key);
    experience.authority = leveler_memory::MemoryAuthority::ToolObservation;
    experience.evidence = Some("task turn-1".to_string());
    experience.experience = Some(leveler_memory::ExperienceRecord {
        category: leveler_memory::ExperienceCategory::RootCause,
        verification: leveler_memory::ExperienceVerification::Verified,
        applies_when: "运行 cargo test 且使用 dev profile".to_string(),
        invalid_if: Some("profile 配置改变".to_string()),
        evidence: vec![
            leveler_memory::ExperienceEvidence {
                kind: "failure".to_string(),
                reference: "step 1: cargo test (exit 101)".to_string(),
            },
            leveler_memory::ExperienceEvidence {
                kind: "resolution".to_string(),
                reference: "step 3: cargo test (exit 0)".to_string(),
            },
        ],
    });
    experience
}

fn memory_tool_names_in(body: &str) -> Vec<&'static str> {
    ["memory", "remember", "forget"]
        .into_iter()
        .filter(|name| body.contains(&format!("\"name\":\"{name}\"")))
        .collect()
}

/// The explicit project switch wins over every permission profile — Full
/// included. It is not an approval question, so Full has nothing to bypass.
#[tokio::test]
async fn disabled_memory_is_not_exposed_under_any_permission_profile() {
    for mode in [
        PermissionProfile::FullAccess,
        PermissionProfile::Assisted,
        PermissionProfile::RequestApproval,
    ] {
        let (tmp, server, app) = harness(Some("false")).await;
        run_chat(&app, mode, "just answer").await;
        let bodies = server.request_bodies().await;
        let first = bodies.first().expect("the agent request must be observed");
        assert!(
            memory_tool_names_in(first).is_empty(),
            "{mode:?} exposed memory tools while the project disabled memory: {first}"
        );
        assert!(
            !first.contains("## Memory"),
            "{mode:?} exposed memory guidance while the project disabled memory: {first}"
        );
        let db = app.open_database().await.unwrap();
        let counts = leveler_storage::MemoryInboxRepository::new(&db)
            .counts()
            .await
            .unwrap();
        assert_eq!(
            counts.pending + counts.processing + counts.failed_retryable + counts.processed,
            0,
            "{mode:?} admitted a turn to memory extraction while disabled"
        );
        assert!(
            !tmp.path().join("state/memory/pending").exists(),
            "{mode:?} created a pending store while disabled"
        );
    }
}

/// With the switch on, the first request already carries the three tools, the
/// guidance, the catalog and the recalled bodies. No `capability(enable)`
/// round trip is required.
#[tokio::test]
async fn enabled_memory_reaches_the_first_request_without_a_capability_round_trip() {
    let (tmp, server, app) = harness(None).await;
    let store = leveler_memory::MemoryStore::open(tmp.path().join("state/memory")).unwrap();
    store
        .activate(
            "Terse output",
            "Keep terminal output compact.",
            leveler_memory::MemoryKind::Preference,
            vec![],
        )
        .unwrap();
    store
        .activate(
            "Chosen database",
            "The project uses SQLite.",
            leveler_memory::MemoryKind::Decision,
            vec![],
        )
        .unwrap();
    store
        .activate(
            "Deploy runbook",
            "The deploy runbook lives in docs/deploy.md.",
            leveler_memory::MemoryKind::Note,
            vec![],
        )
        .unwrap();

    run_chat(
        &app,
        PermissionProfile::Assisted,
        "where is the deploy runbook",
    )
    .await;
    let db = app.open_database().await.unwrap();
    let counts = leveler_storage::MemoryInboxRepository::new(&db)
        .counts()
        .await
        .unwrap();
    assert!(
        counts.pending + counts.processing + counts.failed_retryable + counts.processed > 0,
        "an enabled turn must be admitted to the durable extraction queue"
    );

    let bodies = server.request_bodies().await;
    let first = bodies.first().expect("the agent request must be observed");
    assert_eq!(
        memory_tool_names_in(first),
        vec!["memory", "remember", "forget"],
        "the first request must expose the memory tools: {first}"
    );
    assert!(first.contains("## Memory"), "{first}");
    assert!(
        first.contains("Chosen database"),
        "the catalog must reach the first request: {first}"
    );
    assert!(
        first.contains("Keep terminal output compact."),
        "a standing preference must reach the first request: {first}"
    );
    assert!(
        first.contains("The deploy runbook lives in docs/deploy.md."),
        "query recall must reach the first request: {first}"
    );
}

/// Closing one lane must not close the other. The experience lane keeps its
/// tools and guidance, but nothing in the user lane is admitted, taught or
/// written — including under Full Access.
#[tokio::test]
async fn a_closed_user_lane_keeps_experience_work_and_hides_user_tools() {
    let (_tmp, server, app) = harness(Some(
        "true\n  user_enabled: false\n  experience_enabled: true",
    ))
    .await;
    run_chat(&app, PermissionProfile::FullAccess, "just answer").await;

    let bodies = server.request_bodies().await;
    let first = bodies.first().expect("the agent request must be observed");
    assert_eq!(
        memory_tool_names_in(first),
        vec!["memory", "forget"],
        "a closed user lane must not expose `remember`: {first}"
    );
    assert!(
        !first.contains("/remember"),
        "a closed user lane must not ship its guidance: {first}"
    );
    assert!(
        first.contains("An entry marked `experience`"),
        "the open experience lane still ships its guidance: {first}"
    );

    let db = app.open_database().await.unwrap();
    let user = leveler_storage::MemoryInboxRepository::new(&db)
        .counts()
        .await
        .unwrap();
    assert_eq!(
        user.pending + user.processing + user.failed_retryable + user.processed,
        0,
        "a closed user lane admitted a turn to user-fact extraction"
    );
    let experience = leveler_storage::ExperienceInboxRepository::new(&db)
        .counts()
        .await
        .unwrap();
    assert!(
        experience.pending
            + experience.processing
            + experience.failed_retryable
            + experience.processed
            > 0,
        "the open experience lane must still admit the completed turn"
    );
}

/// The mirror case: user facts keep working while no lesson is admitted,
/// taught or recalled.
#[tokio::test]
async fn a_closed_experience_lane_keeps_user_facts_and_hides_lessons() {
    let (_tmp, server, app) = harness(Some(
        "true\n  user_enabled: true\n  experience_enabled: false",
    ))
    .await;
    let model = ModelRef::new("mock", "m");
    let session = app.create_session(&model, "just answer").await.unwrap();
    run_turn(&app, &session, PermissionProfile::FullAccess, "just answer").await;

    let bodies = server.request_bodies().await;
    let first = bodies.first().expect("the agent request must be observed");
    assert_eq!(
        memory_tool_names_in(first),
        vec!["memory", "remember", "forget"],
        "the open user lane keeps all user tools: {first}"
    );
    assert!(
        first.contains("## Memory"),
        "the open user lane keeps its guidance: {first}"
    );
    assert!(
        !first.contains("An entry marked `experience`"),
        "a closed experience lane must not teach lessons: {first}"
    );

    let db = app.open_database().await.unwrap();
    let user = leveler_storage::MemoryInboxRepository::new(&db)
        .counts()
        .await
        .unwrap();
    assert!(
        user.pending + user.processing + user.failed_retryable + user.processed > 0,
        "the open user lane must still admit the turn"
    );
    let experience = leveler_storage::ExperienceInboxRepository::new(&db)
        .counts()
        .await
        .unwrap();
    assert_eq!(
        experience.pending
            + experience.processing
            + experience.failed_retryable
            + experience.processed,
        0,
        "a closed experience lane admitted a turn to experience extraction"
    );

    // No admitted row means no extractor turn: the lane must not spend a
    // provider request even when the user lane keeps its own extraction.
    let requests = leveler_storage::ModelRequestRepository::new(&db)
        .load_for_session(&session)
        .await
        .unwrap();
    assert!(
        !requests
            .iter()
            .any(|row| row.kind == leveler_storage::ModelCallKind::ExperienceExtraction),
        "a closed experience lane must not call the extractor"
    );
}

/// Old YAML that only declares `memory.enabled: true` must keep user facts
/// working and leave automatic experience closed: the missing key is the
/// product default, not a legacy opt-in.
#[tokio::test]
async fn an_old_config_without_the_experience_key_keeps_lessons_closed() {
    let (_tmp, server, app) = harness(Some("true")).await;
    let model = ModelRef::new("mock", "m");
    let session = app.create_session(&model, "just answer").await.unwrap();
    run_turn(&app, &session, PermissionProfile::FullAccess, "just answer").await;

    let bodies = server.request_bodies().await;
    let first = bodies.first().expect("the agent request must be observed");
    assert_eq!(
        memory_tool_names_in(first),
        vec!["memory", "remember", "forget"],
        "the old YAML keeps the user tools: {first}"
    );
    assert!(
        !first.contains("An entry marked `experience`"),
        "an absent experience key must not teach lessons: {first}"
    );

    let db = app.open_database().await.unwrap();
    let experience = leveler_storage::ExperienceInboxRepository::new(&db)
        .counts()
        .await
        .unwrap();
    assert_eq!(
        experience.pending
            + experience.processing
            + experience.failed_retryable
            + experience.processed,
        0,
        "an absent experience key admitted a turn to experience extraction"
    );
    let requests = leveler_storage::ModelRequestRepository::new(&db)
        .load_for_session(&session)
        .await
        .unwrap();
    assert!(
        !requests
            .iter()
            .any(|row| row.kind == leveler_storage::ModelCallKind::ExperienceExtraction),
        "an absent experience key must not call the extractor"
    );
}

/// Closing a lane is a read/write gate, not a delete. A turn under a closed
/// experience lane must leave the existing lessons where they are, so
/// reopening the lane finds them again.
#[tokio::test]
async fn a_closed_experience_lane_retains_its_entries() {
    let (tmp, _server, app) = harness(Some(
        "true\n  user_enabled: true\n  experience_enabled: false",
    ))
    .await;
    let memory = tmp.path().join("state/memory");
    leveler_memory::MemoryStore::open(&memory)
        .unwrap()
        .admit_experience_candidate(experience_candidate())
        .unwrap();

    run_chat(&app, PermissionProfile::FullAccess, "just answer").await;

    let store = leveler_memory::MemoryStore::open(&memory).unwrap();
    let active = store.effective_active().unwrap();
    assert_eq!(active.len(), 1, "a closed lane deleted its data");
    assert_eq!(
        leveler_memory::MemoryKind::of(&active[0]),
        leveler_memory::MemoryKind::Experience
    );
}

/// `/memory status` reads the same files the worker and the UI use. It must
/// count a legacy sensitive record without exposing it.
#[tokio::test]
async fn status_reports_store_counts_and_sensitive_records() {
    let (tmp, _server, app) = harness(Some(
        "true\n  user_enabled: true\n  experience_enabled: true",
    ))
    .await;
    let memory = tmp.path().join("state/memory");
    let store = leveler_memory::MemoryStore::open(&memory).unwrap();
    store
        .activate(
            "Chosen database",
            "The project uses SQLite.",
            leveler_memory::MemoryKind::Decision,
            vec![],
        )
        .unwrap();
    store
        .admit_experience_candidate(experience_candidate())
        .unwrap();
    let mut legacy = leveler_memory::new_entry(
        "Deploy token",
        "token=ghp_abcdefghijklmnopqrstuvwxyz0123",
        vec![],
    );
    legacy.id = "legacy-deploy-token".into();
    std::fs::write(
        memory.join("active").join(format!("{}.json", legacy.id)),
        serde_json::to_vec_pretty(&legacy).unwrap(),
    )
    .unwrap();

    let status = leveler_app::memory_status::report(&app).await.unwrap();
    assert!(status.total_enabled);
    assert!(status.user_enabled);
    assert!(status.experience_enabled);
    assert!(status.experience_experimental);
    assert_eq!(status.active, 3);
    assert_eq!(
        status.user_active, 2,
        "experience must not inflate user facts"
    );
    assert_eq!(status.pending, 0);
    assert_eq!(status.archived, 0);
    assert_eq!(status.sensitive, 1);
    assert_eq!(status.experience_active, 1);
    assert_eq!(status.experience_verified, 1);
    assert_eq!(status.experience_unverified, 0);
    assert_eq!(status.experience_pending, 0);
    assert_eq!(status.experience_archived, 0);
    assert_eq!(status.experience_inbox_pending, 0);
    assert_eq!(status.inbox_pending, 0);
    assert_eq!(status.inbox_processed, 0);
    assert!(status.last_extraction.is_none());
    assert!(status.last_error.is_none());
}

/// The status strip says how many lessons carry a verified resolution. An
/// observed constraint is real evidence, but no failure later passed, so it
/// must not inflate that count.
#[tokio::test]
async fn status_does_not_count_an_observed_constraint_as_verified() {
    let (tmp, _server, app) = harness(Some(
        "true\n  user_enabled: true\n  experience_enabled: true",
    ))
    .await;
    let memory = tmp.path().join("state/memory");
    let store = leveler_memory::MemoryStore::open(&memory).unwrap();
    let mut candidate = experience_candidate();
    let mut record = candidate.experience.clone().expect("the fixture lesson");
    record.category = leveler_memory::ExperienceCategory::EnvironmentConstraint;
    record.evidence = vec![leveler_memory::ExperienceEvidence {
        kind: "observation".to_string(),
        reference: "step 1: ./verify.sh printed: exit: 0".to_string(),
    }];
    candidate.experience = Some(record);
    store.admit_experience_candidate(candidate).unwrap();

    let status = leveler_app::memory_status::report(&app).await.unwrap();
    assert_eq!(status.experience_active, 1);
    assert_eq!(
        status.experience_verified, 0,
        "an observation-only lesson is not a verified resolution"
    );
    assert_eq!(status.experience_unverified, 1);
}

/// A Goal turn starts under the same policy as Chat: the axis decides what
/// the turn does, not which memory lanes it may see.
#[tokio::test]
async fn a_goal_turn_keeps_the_open_memory_surface() {
    let (_tmp, server, app) = harness_with(
        Some("true\n  user_enabled: true\n  experience_enabled: true"),
        leveler_agent::CollaborationMode::Goal,
    )
    .await;
    let model = ModelRef::new("mock", "m");
    let session = app.create_session(&model, "drive it").await.unwrap();
    // One round is enough to observe the request surface; the goal lifecycle
    // would otherwise keep asking the mock to continue.
    let _ = app
        .run_in_session_bounded(
            &session,
            &model,
            PermissionProfile::Assisted,
            "continue",
            Arc::new(leveler_execution::AutoApprove),
            false,
            &mut |_| {},
            tokio_util::sync::CancellationToken::new(),
            1,
            None,
        )
        .await;
    let bodies = server.request_bodies().await;
    let first = bodies.first().expect("the goal request must be observed");
    assert_eq!(
        memory_tool_names_in(first),
        vec!["memory", "remember", "forget"],
        "a Goal turn lost the open memory tools: {first}"
    );
    assert!(
        first.contains("## Memory"),
        "a Goal turn lost memory guidance: {first}"
    );
    assert!(
        first.contains("An entry marked `experience`"),
        "a Goal turn lost the open experience guidance: {first}"
    );
}

/// A restart re-reads the project policy and stamps it onto the session
/// before the next turn row is inserted. A lane closed in the new process
/// must stay closed for a session created under the old one, while the user
/// lane keeps working — including under Full Access.
#[tokio::test]
async fn a_resumed_session_adopts_the_current_lane_policy() {
    isolate_global_config();
    let server = MockServer::start(vec![text_response("done")]).await;
    let tmp = tempfile::tempdir().unwrap();
    stage_project(
        &tmp,
        &server.base_url(),
        Some("true\n  user_enabled: true\n  experience_enabled: true"),
    );
    let first_app = assemble(&tmp, leveler_agent::CollaborationMode::Chat);
    let model = ModelRef::new("mock", "m");
    let session = first_app
        .create_session(&model, "first turn")
        .await
        .unwrap();
    run_turn(
        &first_app,
        &session,
        PermissionProfile::Assisted,
        "first turn",
    )
    .await;
    let before = experience_rows(&first_app).await;
    assert!(
        before > 0,
        "the open experience lane must admit the first turn"
    );
    let seen = server.request_bodies().await.len();
    drop(first_app);

    stage_project(
        &tmp,
        &server.base_url(),
        Some("true\n  user_enabled: true\n  experience_enabled: false"),
    );
    let resumed_app = assemble(&tmp, leveler_agent::CollaborationMode::Chat);
    run_turn(
        &resumed_app,
        &session,
        PermissionProfile::FullAccess,
        "continue after restart",
    )
    .await;

    let bodies = server.request_bodies().await;
    let resumed = bodies
        .iter()
        .skip(seen)
        .find(|body| body.contains("\"name\":\"memory\""))
        .expect("the resumed chat request must be observed");
    assert_eq!(
        memory_tool_names_in(resumed),
        vec!["memory", "remember", "forget"],
        "the resumed turn lost the open user tools: {resumed}"
    );
    assert!(resumed.contains("## Memory"), "{resumed}");
    assert!(
        !resumed.contains("An entry marked `experience`"),
        "a resumed session resurrected the closed experience lane: {resumed}"
    );
    assert_eq!(
        experience_rows(&resumed_app).await,
        before,
        "a closed experience lane admitted the resumed turn"
    );
}
