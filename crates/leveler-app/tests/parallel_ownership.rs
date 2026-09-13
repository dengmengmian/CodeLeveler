//! The parallel parent's canonical writes are ownership-fenced: exactly the
//! write pattern `parallel_edit` uses (owned EventLog for TaskStarted /
//! CandidateStarted / CandidateFinished, fenced terminal), proven against a
//! stale parent token.

use leveler_core::{OwnerEpoch, RuntimeId};
use leveler_engine::{EngineEvent, EventLog, ExecutionKind, NewSession, TaskEngine};
use leveler_lifecycle::{AgentState, SessionStatus, TaskOutcome};
use leveler_storage::{
    Database, EngineStores, EventStore, OwnershipStore, SessionRepository, SessionStore, TaskStore,
};

fn engine(db: &Database, runtime: &str) -> TaskEngine {
    TaskEngine {
        stores: EngineStores::from_database(db),
        runtime_id: RuntimeId::new(runtime),
    }
}

async fn create_parallel_parent(engine: &TaskEngine) -> leveler_core::SessionId {
    engine
        .create_task(&NewSession {
            workspace: "/repo".into(),
            goal: "parallel goal".into(),
            model: "mock/m".into(),
            mode: "assisted".into(),
            sandbox: false,
            kind: ExecutionKind::Parallel,
            axes: None,
        })
        .await
        .unwrap()
}

#[tokio::test]
async fn parallel_parent_canonical_writes_require_current_owner() {
    let db = Database::connect_in_memory().await.unwrap();
    let engine = engine(&db, "rt-parallel");
    let parent = create_parallel_parent(&engine).await;

    // The parent enters Running through the same engine seam as every task.
    let token = engine
        .mark_running(&parent, AgentState::Execute)
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
    OwnershipStore::acquire(&db, &token.task_id, &engine.runtime_id, token.owner_epoch)
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
    let result = engine
        .finish_task(
            &token,
            &parent,
            leveler_engine::TaskTerminal {
                outcome: TaskOutcome::Failed,
                verification: leveler_lifecycle::VerificationStatus::NotRun,
                reason: Some("stale parent".into()),
                stop: None,
                status: SessionStatus::Failed,
                state: AgentState::Failed,
                goal: None,
            },
            &mut |_| {},
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
    let engine = engine(&db, "rt-a");
    let parent = create_parallel_parent(&engine).await;
    let task = TaskStore::task_for_session(&db, &parent)
        .await
        .unwrap()
        .unwrap();

    // runtime-B owns the task at epoch 1.
    let b = RuntimeId::new("rt-b");
    OwnershipStore::acquire(&db, &task, &b, OwnerEpoch::UNOWNED)
        .await
        .unwrap();

    // runtime-A's parallel parent acquisition must refuse, not CAS-steal.
    let error = engine
        .mark_running(&parent, AgentState::Execute)
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

#[tokio::test]
async fn parallel_parent_finishes_through_the_engine() {
    let db = Database::connect_in_memory().await.unwrap();
    let engine = engine(&db, "rt-parallel");
    let parent = create_parallel_parent(&engine).await;
    let token = engine
        .mark_running(&parent, AgentState::Execute)
        .await
        .unwrap();

    engine
        .finish_task(
            &token,
            &parent,
            leveler_engine::TaskTerminal {
                outcome: TaskOutcome::Completed,
                verification: leveler_lifecycle::VerificationStatus::Passed,
                reason: None,
                stop: None,
                status: SessionStatus::Completed,
                state: AgentState::Complete,
                goal: None,
            },
            &mut |_| {},
        )
        .await
        .unwrap();

    let record = SessionRepository::new(&db)
        .get(&parent)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(record.status, SessionStatus::Completed);
    assert_eq!(record.state, AgentState::Complete);
    assert_eq!(
        SessionStore::execution(&db, &parent).await.unwrap(),
        Some((
            "assisted".into(),
            false,
            ExecutionKind::Parallel.as_str().into(),
            Some(TaskOutcome::Completed),
        ))
    );
    let events = EventStore::load(&db, &parent).await.unwrap();
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].event_type, "task_finished");
}
