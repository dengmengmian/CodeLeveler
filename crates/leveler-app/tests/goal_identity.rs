//! Long-goal P1: a goal has an identity that outlives the turn executing it.
//!
//! These tests assert the identity layer only. One of them asserts what this
//! phase deliberately does NOT do — nothing resumes a goal on its own — because
//! "the runtime quietly started working again" is the failure mode a resume
//! policy exists to prevent, and it must not arrive by accident.

use leveler_core::{SessionId, TaskId};
use leveler_storage::{
    Database, GoalState, GoalStore, SessionRecord, SessionRepository, TaskStore,
};

async fn db_at(path: &std::path::Path) -> Database {
    Database::connect(path).await.unwrap()
}

fn temp_dir(tag: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "leveler-goal-identity-{tag}-{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

async fn seed_task(db: &Database) -> (SessionId, TaskId) {
    let record = SessionRecord::new("/repo", "goal", "mock/m", leveler_core::now());
    SessionRepository::new(db).create(&record).await.unwrap();
    let session = SessionId::new(record.id);
    let task = db
        .ensure_for_session(&session, leveler_core::now())
        .await
        .unwrap();
    (session, task)
}

/// 1. A goal can be created and is immediately identifiable.
#[tokio::test]
async fn a_goal_has_an_identity_the_moment_it_opens() {
    let db = Database::connect_in_memory().await.unwrap();
    let (_, task) = seed_task(&db).await;

    let goal = db
        .open(&task, "add rate limiting to login", leveler_core::now())
        .await
        .unwrap();

    let got = db.get(&goal).await.unwrap().expect("the goal exists");
    assert_eq!(got.objective, "add rate limiting to login");
    assert_eq!(got.state, GoalState::Running);
    assert_eq!(got.task_id, task);
}

/// 2. A goal survives the process that opened it.
///
/// The whole point: a runtime that dies mid-goal must leave something behind
/// that says work is owed.
#[tokio::test]
async fn an_unfinished_goal_is_discoverable_after_a_restart() {
    let dir = temp_dir("restart");
    let path = dir.join("state.db");

    let goal = {
        let db = db_at(&path).await;
        let (_, task) = seed_task(&db).await;
        db.open(&task, "migrate the config format", leveler_core::now())
            .await
            .unwrap()
        // The "process" ends here without settling anything.
    };

    let db = db_at(&path).await;
    let owed = db.unfinished().await.unwrap();
    assert_eq!(owed.len(), 1, "the goal must outlive its process");
    assert_eq!(owed[0].id, goal);
    assert_eq!(owed[0].objective, "migrate the config format");
    assert_eq!(
        owed[0].state,
        GoalState::Running,
        "still owed: nobody settled it"
    );

    std::fs::remove_dir_all(&dir).ok();
}

/// 3. A goal is reachable from its execution history, and back.
#[tokio::test]
async fn a_goal_links_to_the_task_and_session_that_ran_it() {
    let db = Database::connect_in_memory().await.unwrap();
    let (session, task) = seed_task(&db).await;

    let goal = db
        .open(&task, "fix the flaky test", leveler_core::now())
        .await
        .unwrap();

    let record = db.get(&goal).await.unwrap().unwrap();
    assert_eq!(record.task_id, task);
    assert_eq!(
        db.session_for_task(&record.task_id).await.unwrap(),
        Some(session),
        "a goal reaches its transcript through its task"
    );
    assert_eq!(
        db.for_task(&task).await.unwrap().len(),
        1,
        "and the task lists the goals it ran"
    );
}

/// 4. Existing task execution is unchanged: a session that never opened a goal
/// still works, and reports no goals rather than an empty-but-present one.
#[tokio::test]
async fn a_session_without_a_goal_is_untouched() {
    let db = Database::connect_in_memory().await.unwrap();
    let (session, task) = seed_task(&db).await;

    assert!(db.for_task(&task).await.unwrap().is_empty());
    assert!(db.unfinished().await.unwrap().is_empty());
    assert_eq!(
        db.task_for_session(&session).await.unwrap(),
        Some(task),
        "task identity behaves exactly as before"
    );
}

/// 5. Nothing resumes on its own.
///
/// P1 is identity. A goal left `Running` after a restart stays exactly that:
/// discoverable, and untouched. If this test ever fails because a window was
/// recorded or a state changed without a caller asking, a resume policy was
/// implemented by accident — which is the one outcome this phase must not
/// produce.
#[tokio::test]
async fn discovering_an_unfinished_goal_does_not_continue_it() {
    let dir = temp_dir("no-resume");
    let path = dir.join("state.db");

    let goal = {
        let db = db_at(&path).await;
        let (_, task) = seed_task(&db).await;
        db.open(&task, "long refactor", leveler_core::now())
            .await
            .unwrap()
    };

    let db = db_at(&path).await;
    let before = db.get(&goal).await.unwrap().unwrap();

    // Discovery is a read. Do it repeatedly; nothing may move.
    for _ in 0..3 {
        let owed = db.unfinished().await.unwrap();
        assert_eq!(owed.len(), 1);
    }

    let after = db.get(&goal).await.unwrap().unwrap();
    assert_eq!(
        after, before,
        "listing owed work must not advance, settle or re-drive it"
    );
    assert_eq!(after.windows_run, 0, "no window ran");
    assert_eq!(
        after.state,
        GoalState::Running,
        "still owed, still nobody's"
    );

    std::fs::remove_dir_all(&dir).ok();
}

/// One task hosts many goals: the reason this is a table and not columns.
#[tokio::test]
async fn a_second_goal_does_not_overwrite_the_first() {
    let db = Database::connect_in_memory().await.unwrap();
    let (_, task) = seed_task(&db).await;

    let first = db
        .open(&task, "first objective", leveler_core::now())
        .await
        .unwrap();
    db.settle(&first, leveler_core::now()).await.unwrap();
    let second = db
        .open(&task, "second objective", leveler_core::now())
        .await
        .unwrap();

    let all = db.for_task(&task).await.unwrap();
    assert_eq!(all.len(), 2, "goal history is retained, not replaced");
    assert!(
        all.iter()
            .any(|g| g.id == first && g.state == GoalState::Settled)
    );
    assert!(
        all.iter()
            .any(|g| g.id == second && g.state == GoalState::Running)
    );
}
