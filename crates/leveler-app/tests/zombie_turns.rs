//! Regression: kill -9 / unclean TUI exit can leave permanent `running` turns.
//! Opening a new session must reap them so automation and resume stay sane —
//! but only with proof: the boot that ran the turn must have ended.

use std::sync::Arc;

use leveler_app::Application;
use leveler_app::runtime_boot::{RuntimeBootLease, StateDirBootLiveness};
use leveler_client_protocol::{RuntimeEvent, UiUsageScope};
use leveler_core::{SessionId, TurnId};
use leveler_engine::{EngineEvent, TurnKind};
use leveler_model::ModelRef;
use leveler_project::Layout;
use leveler_storage::{
    EngineStores, EventRepository, ModelRequestRecord, ModelRequestRepository, ModelRequestStore,
    SessionRecord, SessionRepository, TurnRecord, TurnRepository,
};

/// Point `LEVELER_HOME` at an empty dir so `GlobalConfig::load()` yields the
/// default. Tests must not depend on the developer's `~/.leveler/config.toml`.
fn isolate_global_config() {
    use std::sync::OnceLock;
    static EMPTY_HOME: OnceLock<tempfile::TempDir> = OnceLock::new();
    let dir = EMPTY_HOME.get_or_init(|| tempfile::tempdir().unwrap());
    unsafe {
        std::env::set_var("LEVELER_HOME", dir.path());
    }
}

async fn app_with_session() -> (tempfile::TempDir, Application, SessionId) {
    isolate_global_config();
    let tmp = tempfile::tempdir().unwrap();
    let layout = Layout::from_parts(
        tmp.path().to_path_buf(),
        tmp.path().join("configs"),
        tmp.path().join("state"),
    );
    let app = Application::assemble(layout).expect("assemble with empty config");
    let db = app.open_database().await.expect("open db");
    let zombie_session = SessionRecord::new(
        tmp.path().display().to_string(),
        "old goal",
        "mock/m",
        leveler_core::now(),
    );
    SessionRepository::new(&db)
        .create(&zombie_session)
        .await
        .unwrap();
    let session_id = SessionId::new(zombie_session.id.clone());
    (tmp, app, session_id)
}

/// A turn started under ownership of `lease`'s boot, exactly as a live
/// process starts one.
async fn turn_of_boot(app: &Application, session: &SessionId, lease: &RuntimeBootLease) {
    let db = app.open_database().await.unwrap();
    let engine = leveler_engine::TaskEngine {
        stores: leveler_storage::EngineStores::from_database(&db),
        runtime_id: app.runtime_id().unwrap(),
        boot: leveler_engine::EngineBoot {
            id: lease.id().clone(),
            liveness: Arc::new(StateDirBootLiveness::new(&app.layout.state_dir)),
        },
    };
    let token = engine.acquire_ownership(session).await.unwrap();
    engine
        .stores
        .turns
        .start_owned(&token, session, "chat", None, leveler_core::now())
        .await
        .unwrap();
}

async fn only_turn(app: &Application, session: &SessionId) -> TurnRecord {
    let db = app.open_database().await.unwrap();
    let mut turns = TurnRepository::new(&db).list(session).await.unwrap();
    assert_eq!(turns.len(), 1);
    turns.remove(0)
}

/// The process that ran the turn is gone — its boot lease released, as the OS
/// does for a killed process. Creating a session settles the dead turn.
#[tokio::test]
async fn create_session_reaps_a_dead_boots_running_turn() {
    let (_tmp, app, session_id) = app_with_session().await;
    let crashed = RuntimeBootLease::acquire(&app.layout.state_dir).unwrap();
    turn_of_boot(&app, &session_id, &crashed).await;
    let crashed_boot = crashed.id().as_str().to_string();
    drop(crashed);

    app.create_session(&ModelRef::new("mock", "m"), "fresh session")
        .await
        .expect("create_session must succeed");

    let after = only_turn(&app, &session_id).await;
    assert_eq!(
        after.status, "interrupted",
        "a dead boot's running turn must be reaped when a new session is created"
    );
    assert!(after.finished_at.is_some());
    assert_eq!(after.owner_boot_id.as_deref(), Some(crashed_boot.as_str()));
}

/// T8: a turn left running before boots were recorded cannot be proven
/// orphaned. It stays running rather than be interrupted on a guess.
#[tokio::test]
async fn create_session_leaves_a_running_turn_without_a_boot_alone() {
    let (_tmp, app, session_id) = app_with_session().await;
    let db = app.open_database().await.unwrap();
    TurnRepository::new(&db)
        .start(&session_id, "chat", None, leveler_core::now())
        .await
        .unwrap();

    app.create_session(&ModelRef::new("mock", "m"), "fresh session")
        .await
        .expect("create_session must succeed");

    let after = only_turn(&app, &session_id).await;
    assert_eq!(after.status, "running");
    assert!(after.finished_at.is_none());
}

/// T15: the boot's lock cannot be probed. That is no proof of death.
#[cfg(unix)]
#[tokio::test]
async fn create_session_leaves_a_turn_whose_boot_cannot_be_probed_alone() {
    use std::os::unix::fs::PermissionsExt;

    let (_tmp, app, session_id) = app_with_session().await;
    let unreadable = RuntimeBootLease::acquire(&app.layout.state_dir).unwrap();
    turn_of_boot(&app, &session_id, &unreadable).await;
    let lock = app
        .layout
        .state_dir
        .join("boots")
        .join(format!("{}.lock", unreadable.id().as_str()));
    drop(unreadable);
    std::fs::write(&lock, b"").unwrap();
    std::fs::set_permissions(&lock, std::fs::Permissions::from_mode(0o000)).unwrap();

    let created = app
        .create_session(&ModelRef::new("mock", "m"), "fresh session")
        .await;
    std::fs::set_permissions(&lock, std::fs::Permissions::from_mode(0o600)).unwrap();
    created.expect("create_session must succeed");

    let after = only_turn(&app, &session_id).await;
    assert_eq!(after.status, "running");
}

/// A crash between a measured provider call and the turn terminal: the reaper
/// settles the row as interrupted WITHOUT inventing a usage projection, and
/// reopening the session rebuilds that projection from the persisted ledger.
/// Replay and reporting are pure reads — no second settlement, no double count.
#[tokio::test]
async fn a_reaped_turns_measured_usage_replays_once_from_the_ledger() {
    let (_tmp, app, session_id) = app_with_session().await;
    let crashed = RuntimeBootLease::acquire(&app.layout.state_dir).unwrap();
    turn_of_boot(&app, &session_id, &crashed).await;
    let turn = only_turn(&app, &session_id).await;
    let turn_id = TurnId::new(turn.id.clone());
    let db = app.open_database().await.unwrap();

    let (kind, payload) = EngineEvent::TurnStarted {
        turn_id: turn_id.clone(),
        kind: TurnKind::Chat,
    }
    .to_row()
    .unwrap();
    EventRepository::new(&db)
        .append(
            &session_id,
            Some(&turn_id),
            &kind,
            &payload,
            leveler_core::now(),
        )
        .await
        .unwrap();

    // Two billed calls the provider already answered; the process died before
    // the turn could settle them into its terminal.
    for (input, output, cached) in [(1_000_u64, 100_u64, 900_u64), (2_000, 200, 1_800)] {
        ModelRequestRepository::new(&db)
            .insert(&ModelRequestRecord {
                budget_scope: None,
                estimated_tokens: None,
                id: leveler_core::EventId::generate().into_inner(),
                provider_request_id: None,
                session_id: session_id.clone(),
                provider: "deepseek".into(),
                model: "m".into(),
                input_tokens: input,
                output_tokens: output,
                cached_input_tokens: Some(cached),
                reasoning_tokens: None,
                projected_input_tokens: None,
                projected_reasoning_tokens: None,
                cost_usd_micros: None,
                agent_id: None,
                finish_reason: Some("stop".into()),
                error_kind: None,
                error_detail: None,
                turn_id: Some(turn_id.as_str().to_string()),
                latency_ms: None,
                attempt_ms: None,
                connect_ms: None,
                ttft_ms: None,
                max_event_gap_ms: None,
                retry_count: 0,
                kind: leveler_storage::ModelCallKind::Round,
                created_at: leveler_core::now(),
                reasoning_effort: None,
            })
            .await
            .unwrap();
    }
    drop(crashed);

    // Opening a fresh session is what proves the dead boot and reaps the turn.
    app.create_session(&ModelRef::new("mock", "m"), "fresh session")
        .await
        .expect("create_session must succeed");

    let stores = EngineStores::from_database(&db);
    let terminal = stores
        .events
        .load_last_by_type(&session_id, "turn_finished", None)
        .await
        .unwrap()
        .expect("the reaper must write a turn terminal");
    assert!(
        !terminal.payload.contains("\"usage\""),
        "the reaper has no ledger read of its own and must not fabricate one: {}",
        terminal.payload
    );

    // Settlement is booked, not recomputed: replay and usage queries below
    // may read these rows but must never add a ledger or terminal row.
    let before_rows = db.load_for_turn(&session_id, &turn_id).await.unwrap().len();
    assert_eq!(before_rows, 2, "the two provider calls remain two rows");
    let events_before = EventRepository::new(&db)
        .headers(&session_id)
        .await
        .unwrap()
        .len();

    let (entries, _) = leveler_app::session_history::load_session_history(&db, &session_id)
        .await
        .unwrap();
    let usage = entries
        .iter()
        .find_map(|entry| match &entry.event {
            RuntimeEvent::TurnUsage { usage } => Some(usage.clone()),
            _ => None,
        })
        .expect("reopening the session must project the settled ledger");
    assert_eq!(
        (
            usage.requests,
            usage.unmeasured_requests,
            usage.input_tokens,
            usage.output_tokens,
            usage.cached_input_tokens
        ),
        (2, 0, 3_000, 300, Some(2_700)),
        "both booked calls are read exactly once"
    );

    // Settlement is booked, not recomputed: neither replay nor two usage
    // queries may add a ledger row or change the total.
    let first = leveler_app::usage::query_usage(&db, &session_id, UiUsageScope::Session, 0)
        .await
        .unwrap();
    let second = leveler_app::usage::query_usage(&db, &session_id, UiUsageScope::Session, 0)
        .await
        .unwrap();
    assert_eq!(
        (
            first.requests,
            first.unmeasured_requests,
            first.input_tokens,
            first.output_tokens,
            first.cached_input_tokens,
            first.cost_usd_micros,
            first.priced_requests,
            first.models.as_slice()
        ),
        (
            second.requests,
            second.unmeasured_requests,
            second.input_tokens,
            second.output_tokens,
            second.cached_input_tokens,
            second.cost_usd_micros,
            second.priced_requests,
            second.models.as_slice()
        ),
        "re-asking is the same durable projection; only generated_at moves"
    );
    assert_eq!(
        (
            second.requests,
            second.unmeasured_requests,
            second.total_tokens()
        ),
        (2, 0, 3_300)
    );
    assert_eq!(
        db.load_for_turn(&session_id, &turn_id).await.unwrap().len(),
        2,
        "replay and reporting never settle usage a second time"
    );
    assert_eq!(
        EventRepository::new(&db)
            .headers(&session_id)
            .await
            .unwrap()
            .len(),
        events_before,
        "replay is a read: it appends no duplicate terminal or settlement row"
    );
    let turn_report = leveler_app::usage::query_usage(&db, &session_id, UiUsageScope::Turn, 0)
        .await
        .unwrap();
    assert_eq!(
        (turn_report.requests, turn_report.total_tokens()),
        (2, 3_300),
        "the Turn scope reads the same rows as the settled receipt"
    );
}
