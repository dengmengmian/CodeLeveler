//! The parallel parent's canonical writes are ownership-fenced: exactly the
//! write pattern `parallel_edit` uses (owned EventLog for TaskStarted /
//! CandidateStarted / CandidateFinished, fenced terminal), proven against a
//! stale parent token.

use leveler_core::{OwnerEpoch, RuntimeId, SessionId};
use leveler_engine::{EngineEvent, EventLog, ExecutionKind};
use leveler_lifecycle::{AgentState, SessionStatus, TaskOutcome};
use leveler_storage::{
    Database, EventStore, OwnershipStore, SessionRecord, SessionStore, TaskStore, TerminalStore,
};

#[tokio::test]
async fn parallel_parent_canonical_writes_require_current_owner() {
    let db = Database::connect_in_memory().await.unwrap();
    let record = SessionRecord::new("/repo", "parallel goal", "mock/m", leveler_core::now());
    let parent = SessionId::new(record.id.clone());
    SessionStore::create(&db, &record).await.unwrap();
    let task = TaskStore::ensure_for_session(&db, &parent, leveler_core::now())
        .await
        .unwrap();
    let rt = RuntimeId::new("rt-parallel");

    // The parent acquires ownership (as parallel_edit now does)…
    let token = OwnershipStore::acquire(&db, &task, &rt, OwnerEpoch::UNOWNED)
        .await
        .unwrap();
    SessionStore::update_status_owned(
        &db,
        &token,
        &parent,
        SessionStatus::Running,
        AgentState::Execute,
        leveler_core::now(),
    )
    .await
    .unwrap();
    let owned_log = EventLog::new_owned(&db, parent.clone(), token.clone());
    let sink = &mut |_: EngineEvent| {};
    owned_log
        .append(
            None,
            EngineEvent::TaskStarted {
                goal: "parallel goal".into(),
                model: "mock/m".into(),
                mode: "assisted".into(),
                sandbox: false,
                kind: ExecutionKind::Parallel,
                task_id: Some(token.task_id.clone()),
            },
            sink,
        )
        .await
        .unwrap();
    owned_log
        .append(
            None,
            EngineEvent::CandidateStarted {
                branch: "b-0".into(),
            },
            sink,
        )
        .await
        .unwrap();

    // …then loses it (a newer epoch exists).
    OwnershipStore::acquire(&db, &task, &rt, token.owner_epoch)
        .await
        .unwrap();
    let events_before = EventStore::load(&db, &parent).await.unwrap().len();

    // Stale CandidateFinished append: refused, nothing stored, observer
    // never sees it (persist-before-forward).
    let mut forwarded = 0usize;
    let result = owned_log
        .append(
            None,
            EngineEvent::CandidateFinished {
                branch: "b-0".into(),
                session_id: String::new(),
                verified: true,
            },
            &mut |_| forwarded += 1,
        )
        .await;
    assert!(result.is_err(), "stale CandidateFinished must be refused");
    assert_eq!(forwarded, 0, "no observer forward without persistence");

    // Stale terminal: refused atomically — no event, no projection.
    let result = TerminalStore::finish_task_owned(
        &db,
        &token,
        &parent,
        "task_finished",
        "{}",
        TaskOutcome::Failed,
        SessionStatus::Failed,
        AgentState::Failed,
        leveler_core::now(),
    )
    .await;
    assert!(result.is_err(), "stale terminal must be refused");
    assert_eq!(
        EventStore::load(&db, &parent).await.unwrap().len(),
        events_before
    );
    let (_, _, _, outcome) = SessionStore::execution(&db, &parent)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(outcome, None, "no stale outcome mutation");
}

/// A foreign-owned task must never be auto-stolen by the parallel parent
/// path: explicit error, owner and epoch untouched, session not Running,
/// canonical log empty.
#[tokio::test]
async fn parallel_parent_refuses_foreign_owner() {
    let db = Database::connect_in_memory().await.unwrap();
    let record = SessionRecord::new("/repo", "parallel goal", "mock/m", leveler_core::now());
    let parent = SessionId::new(record.id.clone());
    SessionStore::create(&db, &record).await.unwrap();
    let task = TaskStore::ensure_for_session(&db, &parent, leveler_core::now())
        .await
        .unwrap();

    // runtime-B owns the task at epoch 1.
    let b = RuntimeId::new("rt-b");
    OwnershipStore::acquire(&db, &task, &b, OwnerEpoch::UNOWNED)
        .await
        .unwrap();

    // runtime-A's parallel parent acquisition must refuse, not CAS-steal.
    let error = leveler_app::acquire_parallel_parent_ownership(&db, &task, &RuntimeId::new("rt-a"))
        .await
        .expect_err("a foreign owner must never be auto-stolen");
    assert!(
        error.to_string().contains("owned by runtime"),
        "must be a named conflict: {error}"
    );

    // Owner, epoch, session status, and canonical log are all untouched.
    let current = OwnershipStore::current(&db, &task).await.unwrap().unwrap();
    assert_eq!(current.runtime.as_ref(), Some(&b));
    assert_eq!(current.epoch.get(), 1);
    let (_, _, _, outcome) = SessionStore::execution(&db, &parent)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(outcome, None);
    let record = leveler_storage::SessionRepository::new(&db)
        .get(&parent)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        record.status,
        SessionStatus::Created,
        "the session must not have entered Running"
    );
    assert!(EventStore::load(&db, &parent).await.unwrap().is_empty());
}
