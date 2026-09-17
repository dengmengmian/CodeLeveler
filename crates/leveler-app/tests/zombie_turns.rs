//! Regression: kill -9 / unclean TUI exit can leave permanent `running` turns.
//! Opening a new session must reap them so automation and resume stay sane —
//! but only with proof: the boot that ran the turn must have ended.

use std::sync::Arc;

use leveler_app::Application;
use leveler_app::runtime_boot::{RuntimeBootLease, StateDirBootLiveness};
use leveler_core::SessionId;
use leveler_model::ModelRef;
use leveler_project::Layout;
use leveler_storage::{SessionRecord, SessionRepository, TurnRecord, TurnRepository};

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
