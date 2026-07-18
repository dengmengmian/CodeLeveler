//! Legacy remote-readiness regression tests. These protect useful log-level
//! properties but do not by themselves claim that the runtime is production-
//! or ready for a remote transport.

use leveler_core::{ApprovalId, SessionId};
use leveler_engine::{EngineEvent, EventLog, TaskOutcome};
use leveler_storage::{Database, MemoryEventStore, SessionRecord, SessionRepository};

/// #2: waiting on approval, then a restart — the pending request still exists.
/// An ApprovalRequested with no matching ApprovalResolved is durable in the
/// event log and is recovered on replay.
#[tokio::test]
async fn pending_approval_survives_a_restart_via_the_event_log() {
    let db = Database::connect_in_memory().await.unwrap();
    let record = SessionRecord::new("/repo", "goal", "mock/m", leveler_core::now());
    SessionRepository::new(&db).create(&record).await.unwrap();
    let session = SessionId::new(record.id);

    // A run requests approval, then the process dies before it is answered.
    {
        let log = EventLog::new(&db, session.clone());
        log.append(
            None,
            EngineEvent::ApprovalRequested {
                id: ApprovalId::generate(),
                tool: "run_command".into(),
                summary: "run tests".into(),
                command: Some("cargo test".into()),
                risk: "assisted".into(),
            },
            &mut |_| {},
        )
        .await
        .unwrap();
    }

    // Restart: a fresh log replays and still finds the unanswered approval.
    let log = EventLog::new(&db, session);
    let replayed = log.replay().await.unwrap();
    let pending = replayed
        .iter()
        .filter(|e| matches!(e, EngineEvent::ApprovalRequested { .. }))
        .count();
    let resolved = replayed
        .iter()
        .filter(|e| matches!(e, EngineEvent::ApprovalResolved { .. }))
        .count();
    assert_eq!(pending, 1, "the pending approval must survive the restart");
    assert_eq!(resolved, 0, "it must still be unanswered");
}

/// #8: a verification failure can never become an automation success. Only
/// `Verified` is shippable; a completed-but-unverified run is not.
#[tokio::test]
async fn verifier_failure_is_never_an_automation_success() {
    assert!(TaskOutcome::Verified.is_success());
    assert!(!TaskOutcome::CompletedUnverified.is_success());
    assert!(!TaskOutcome::Failed.is_success());
    assert!(!TaskOutcome::Interrupted.is_success());
}

/// #6 / #9: transient deltas carry no replay value and are never persisted, so
/// losing them costs nothing; the canonical terminal event is always
/// recoverable by replay — the basis for snapshot resync.
#[tokio::test]
async fn transient_loss_is_harmless_and_canonical_events_replay() {
    let store = MemoryEventStore::new();
    let log = EventLog::new(&store, SessionId::generate());

    log.append(
        None,
        EngineEvent::AssistantDelta {
            text: "streamed".into(),
        },
        &mut |_| {},
    )
    .await
    .unwrap();
    log.append(
        None,
        EngineEvent::TaskFinished {
            outcome: TaskOutcome::Verified,
            reason: None,
        },
        &mut |_| {},
    )
    .await
    .unwrap();

    let replayed = log.replay().await.unwrap();
    assert_eq!(
        replayed,
        vec![EngineEvent::TaskFinished {
            outcome: TaskOutcome::Verified,
            reason: None,
        }],
        "transient delta dropped; canonical completion recovered"
    );
}
