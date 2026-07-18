//! Regression: kill -9 / unclean TUI exit can leave permanent `running` turns.
//! Opening a new session must reaper them so automation and resume stay sane.

use leveler_app::Application;
use leveler_core::SessionId;
use leveler_model::ModelRef;
use leveler_project::Layout;
use leveler_storage::{SessionRecord, SessionRepository, TurnRepository};

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

#[tokio::test]
async fn create_session_reaps_zombie_running_turns() {
    isolate_global_config();
    let tmp = tempfile::tempdir().unwrap();
    let layout = Layout {
        repo_root: tmp.path().to_path_buf(),
        config_dir: tmp.path().join("configs"),
        state_dir: tmp.path().join("state"),
    };
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
    TurnRepository::new(&db)
        .start(&session_id, "chat", None, leveler_core::now())
        .await
        .unwrap();

    let before = TurnRepository::new(&db).list(&session_id).await.unwrap();
    assert_eq!(before[0].status, "running");
    assert!(before[0].finished_at.is_none());

    app.create_session(&ModelRef::new("mock", "m"), "fresh session")
        .await
        .expect("create_session must succeed");

    let after = TurnRepository::new(&db).list(&session_id).await.unwrap();
    assert_eq!(
        after[0].status, "interrupted",
        "zombie running turn must be reaped when a new session is created"
    );
    assert!(after[0].finished_at.is_some());
}
