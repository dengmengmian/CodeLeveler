//! Fenced-write contract: a stale OwnershipToken stores NOTHING through any
//! authoritative port (Scenarios D/E/F/G), a current token works normally
//! (Scenario I), and both adapters — SQLite (real conditional statements /
//! transactions) and memory (ownership lock held across check+write) — honor
//! the same observable semantics.

use std::sync::Arc;

use leveler_core::{OwnerEpoch, OwnershipToken, RuntimeId, SessionId, TaskId, TurnId};
use leveler_lifecycle::{AgentState, SessionStatus, TaskOutcome, TurnOutcome};
use leveler_storage::{
    Database, EventStore, MemoryEventStore, MemoryMessageStore, MemoryOwnershipState,
    MemoryOwnershipStore, MemorySessionStore, MemoryTerminalStore, MemoryTurnStore, MessageStore,
    OwnershipError, OwnershipStore, SessionRecord, SessionStore, TerminalStore, TurnStore,
};

struct Ports<'a> {
    events: &'a dyn EventStore,
    turns: &'a dyn TurnStore,
    messages: &'a dyn MessageStore,
    sessions: &'a dyn SessionStore,
    terminal: &'a dyn TerminalStore,
}

fn assert_stale<T: std::fmt::Debug>(result: Result<T, OwnershipError>, what: &str) {
    match result {
        Err(OwnershipError::Stale { .. }) => {}
        other => panic!("{what} with a stale token must be typed Stale, got {other:?}"),
    }
}

/// One contract: every fenced write refuses the stale token with nothing
/// stored, then succeeds with the current token.
async fn assert_fencing_contract(
    ports: Ports<'_>,
    session: &SessionId,
    stale: &OwnershipToken,
    current: &OwnershipToken,
) {
    let now = leveler_core::now;

    // Scenario D: stale canonical append → typed error, log untouched.
    assert_stale(
        ports
            .events
            .append_owned(stale, session, None, "task_started", "{}", now())
            .await,
        "event append",
    );
    assert!(
        ports.events.load(session).await.unwrap().is_empty(),
        "a stale append must store no event"
    );

    // Scenario F: stale turn start → no turn row.
    assert_stale(
        ports
            .turns
            .start_owned(stale, session, "user", None, now())
            .await,
        "turn start",
    );
    assert!(
        ports
            .turns
            .list_running(Some(session))
            .await
            .unwrap()
            .is_empty()
    );

    // Scenario I: the current owner proceeds normally.
    let turn = ports
        .turns
        .start_owned(current, session, "user", None, now())
        .await
        .unwrap();
    let turn_id = TurnId::new(turn.id.clone());
    ports
        .events
        .append_owned(
            current,
            session,
            Some(&turn_id),
            "turn_started",
            "{}",
            now(),
        )
        .await
        .unwrap();
    ports
        .messages
        .append_in_turn_owned(current, session, &turn_id, &["msg-1".to_string()], now())
        .await
        .unwrap();

    // Scenario G: stale transcript append → nothing stored.
    assert_stale(
        ports
            .messages
            .append_in_turn_owned(
                stale,
                session,
                &turn_id,
                &["SHOULD_NOT_LAND".to_string()],
                now(),
            )
            .await,
        "transcript append",
    );
    let transcript = ports.messages.load(session).await.unwrap();
    assert_eq!(
        transcript,
        vec!["msg-1"],
        "stale transcript writes must not land"
    );

    // Stale Running transition refused.
    assert_stale(
        ports
            .sessions
            .update_status_owned(
                stale,
                session,
                SessionStatus::Running,
                AgentState::Execute,
                now(),
            )
            .await,
        "status update",
    );
    ports
        .sessions
        .update_status_owned(
            current,
            session,
            SessionStatus::Running,
            AgentState::Execute,
            now(),
        )
        .await
        .unwrap();

    // Scenario E: stale terminal commits roll back atomically — no event, no
    // projection mutation.
    let events_before = ports.events.load(session).await.unwrap().len();
    assert_stale(
        ports
            .terminal
            .finish_turn_owned(
                stale,
                session,
                &turn_id,
                "turn_finished",
                "{}",
                TurnOutcome::Interrupted,
                now(),
            )
            .await,
        "terminal turn commit",
    );
    assert_stale(
        ports
            .terminal
            .finish_task_owned(
                stale,
                session,
                "task_finished",
                "{}",
                TaskOutcome::Failed,
                SessionStatus::Failed,
                AgentState::Failed,
                now(),
            )
            .await,
        "terminal task commit",
    );
    assert_eq!(
        ports.events.load(session).await.unwrap().len(),
        events_before
    );
    assert_eq!(
        ports.turns.list_running(Some(session)).await.unwrap().len(),
        1,
        "the turn must still be running after a stale terminal attempt"
    );
    let (_, _, _, outcome) = ports.sessions.execution(session).await.unwrap().unwrap();
    assert_eq!(outcome, None, "no outcome from a stale terminal attempt");

    // Scenario I (terminal): the current owner commits normally.
    ports
        .terminal
        .finish_turn_owned(
            current,
            session,
            &turn_id,
            "turn_finished",
            "{}",
            TurnOutcome::Completed,
            now(),
        )
        .await
        .unwrap();
    ports
        .terminal
        .finish_task_owned(
            current,
            session,
            "task_finished",
            "{}",
            TaskOutcome::CompletedUnverified,
            SessionStatus::Completed,
            AgentState::Complete,
            now(),
        )
        .await
        .unwrap();
    let (_, _, _, outcome) = ports.sessions.execution(session).await.unwrap().unwrap();
    assert_eq!(outcome, Some(TaskOutcome::CompletedUnverified));
}

#[tokio::test]
async fn sqlite_fenced_writes_honor_the_contract() {
    let db = Database::connect_in_memory().await.unwrap();
    let record = SessionRecord::new("/repo", "goal", "mock/m", leveler_core::now());
    let session = SessionId::new(record.id.clone());
    SessionStore::create(&db, &record).await.unwrap();
    let task = leveler_storage::TaskStore::ensure_for_session(&db, &session, leveler_core::now())
        .await
        .unwrap();
    let rt = RuntimeId::new("rt-a");
    // Epoch 1 then reacquire to epoch 2: the epoch-1 token is stale.
    let stale = OwnershipStore::acquire(&db, &task, &rt, OwnerEpoch::UNOWNED)
        .await
        .unwrap();
    let current = OwnershipStore::acquire(&db, &task, &rt, stale.owner_epoch)
        .await
        .unwrap();
    assert_fencing_contract(
        Ports {
            events: &db,
            turns: &db,
            messages: &db,
            sessions: &db,
            terminal: &db,
        },
        &session,
        &stale,
        &current,
    )
    .await;
}

#[tokio::test]
async fn memory_fenced_writes_honor_the_contract() {
    let state = Arc::new(MemoryOwnershipState::new());
    // Memory task ids equal session ids by construction (MemoryTaskStore rule).
    let session = SessionId::new("session-1");
    let task = TaskId::new("session-1");
    state.register_task(&task);

    let sessions = Arc::new(MemorySessionStore::new().with_ownership(state.clone()));
    let turns = Arc::new(MemoryTurnStore::new().with_ownership(state.clone()));
    let events = Arc::new(MemoryEventStore::new().with_ownership(state.clone()));
    let messages = MemoryMessageStore::new().with_ownership(state.clone());
    let terminal = MemoryTerminalStore::new(sessions.clone(), turns.clone(), events.clone())
        .with_ownership(state.clone());

    let record = SessionRecord {
        id: session.as_str().to_string(),
        ..SessionRecord::new("/repo", "goal", "mock/m", leveler_core::now())
    };
    SessionStore::create(sessions.as_ref(), &record)
        .await
        .unwrap();

    let ownership = MemoryOwnershipStore::new(state);
    let rt = RuntimeId::new("rt-a");
    let stale = ownership
        .acquire(&task, &rt, OwnerEpoch::UNOWNED)
        .await
        .unwrap();
    let current = ownership
        .acquire(&task, &rt, stale.owner_epoch)
        .await
        .unwrap();

    assert_fencing_contract(
        Ports {
            events: events.as_ref(),
            turns: turns.as_ref(),
            messages: &messages,
            sessions: sessions.as_ref(),
            terminal: &terminal,
        },
        &session,
        &stale,
        &current,
    )
    .await;
}
