//! Long-goal P3 contract tests: cursor exactness, checkpoint+delta resume,
//! idempotent triggers, corrupt-checkpoint fail-closed, and the interruption
//! trigger — all against real SQLite (in-memory), never mocked stores.

use leveler_core::{RuntimeId, SessionId};
use leveler_engine::{
    EngineEvent, create_goal_checkpoint, reap_after_restart, resume_prior_from_checkpoint,
};
use leveler_lifecycle::CheckpointReason;
use leveler_model::{Message, Role};
use leveler_storage::{
    Database, EngineStores, GoalStore, MessageRepository, SessionRecord, SessionRepository,
    TaskStore, TurnRepository,
};

struct Fixture {
    db: Database,
    stores: EngineStores,
    session: SessionId,
    goal: leveler_core::GoalId,
}

async fn fixture() -> Fixture {
    let db = Database::connect_in_memory().await.unwrap();
    let record = SessionRecord::new("/repo", "port the parser", "mock/m", leveler_core::now());
    SessionRepository::new(&db).create(&record).await.unwrap();
    let session = SessionId::new(record.id);
    let task = db
        .ensure_for_session(&session, leveler_core::now())
        .await
        .unwrap();
    let goal = GoalStore::open(&db, &task, "port the parser", leveler_core::now())
        .await
        .unwrap();
    let stores = EngineStores::from_database(&db);
    Fixture {
        db,
        stores,
        session,
        goal,
    }
}

async fn append_event(fx: &Fixture, event: EngineEvent) -> i64 {
    let (event_type, payload) = event.to_row().unwrap();
    leveler_storage::EventStore::append(
        &fx.db,
        &fx.session,
        None,
        &event_type,
        &payload,
        leveler_core::now(),
    )
    .await
    .unwrap()
    .sequence
}

fn marker_event(detail: &str) -> EngineEvent {
    EngineEvent::GoalIntercepted {
        kind: "test".into(),
        detail: detail.into(),
    }
}

async fn append_messages(fx: &Fixture, texts: &[&str]) {
    let turn = TurnRepository::new(&fx.db)
        .start(&fx.session, "chat", None, leveler_core::now())
        .await
        .unwrap();
    let payloads: Vec<String> = texts
        .iter()
        .map(|t| serde_json::to_string(&Message::text(Role::User, t.to_string())).unwrap())
        .collect();
    MessageRepository::new(&fx.db)
        .append_in_turn(
            &fx.session,
            &leveler_core::TurnId::new(turn.id),
            &payloads,
            leveler_core::now(),
        )
        .await
        .unwrap();
}

fn message_texts(messages: &[Message]) -> Vec<String> {
    messages.iter().map(|m| m.text_content()).collect()
}

/// §58/§67: the cursor is the committed boundary at creation time, and
/// resume receives the checkpoint block plus EXACTLY the messages after the
/// watermark — no duplication of what the checkpoint represents, no gap.
#[tokio::test]
async fn resume_receives_checkpoint_plus_exact_delta() {
    let fx = fixture().await;
    append_messages(&fx, &["before-1", "before-2"]).await;
    append_event(&fx, marker_event("e1")).await;
    let e2 = append_event(&fx, marker_event("e2")).await;

    let record = create_goal_checkpoint(
        &fx.stores,
        &fx.session,
        CheckpointReason::Manual,
        None,
        None,
    )
    .await
    .unwrap()
    .expect("a goal exists");
    assert_eq!(
        record.event_cursor, e2,
        "the cursor is the committed MAX(sequence) at creation"
    );
    assert_eq!(record.payload.transcript_ordinal, Some(2));

    // Work continues after the checkpoint: the recent delta.
    append_event(&fx, marker_event("e3")).await;
    append_messages(&fx, &["after-1"]).await;

    let transcript = leveler_engine::RawTranscript::load_strict(
        fx.stores.messages.as_ref(),
        &fx.session,
        "test transcript",
    )
    .await
    .unwrap();
    let prior = resume_prior_from_checkpoint(&fx.stores, &fx.session, &transcript)
        .await
        .unwrap()
        .expect("a valid checkpoint must be consumed");

    let texts = message_texts(&prior);
    assert!(
        texts[0].starts_with("[GOAL CHECKPOINT]"),
        "resume context leads with the checkpoint block: {:?}",
        texts[0]
    );
    assert_eq!(
        &texts[1..],
        &["after-1".to_string()],
        "exactly the post-watermark delta — nothing replayed, nothing skipped"
    );
    assert!(
        !texts.iter().any(|t| t == "before-1" || t == "before-2"),
        "messages the checkpoint represents must not be replayed"
    );
}

/// §61: the same boundary re-triggered is ONE logical checkpoint; a new
/// boundary is a new checkpoint.
#[tokio::test]
async fn repeated_trigger_at_same_boundary_is_one_checkpoint() {
    let fx = fixture().await;
    append_event(&fx, marker_event("e1")).await;
    let first = create_goal_checkpoint(
        &fx.stores,
        &fx.session,
        CheckpointReason::Manual,
        None,
        None,
    )
    .await
    .unwrap()
    .unwrap();
    let repeat = create_goal_checkpoint(
        &fx.stores,
        &fx.session,
        CheckpointReason::Manual,
        None,
        None,
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(
        first.id, repeat.id,
        "a retry must not mint a second checkpoint"
    );

    append_event(&fx, marker_event("e2")).await;
    let advanced = create_goal_checkpoint(
        &fx.stores,
        &fx.session,
        CheckpointReason::Manual,
        None,
        None,
    )
    .await
    .unwrap()
    .unwrap();
    assert_ne!(advanced.id, first.id, "a new boundary is a new checkpoint");
    assert!(advanced.event_cursor > first.event_cursor);
}

/// §35 backward compatibility: no checkpoint → the caller's pre-P3 path.
#[tokio::test]
async fn no_checkpoint_yields_no_prior() {
    let fx = fixture().await;
    append_messages(&fx, &["hello"]).await;
    let transcript = leveler_engine::RawTranscript::load_strict(
        fx.stores.messages.as_ref(),
        &fx.session,
        "test transcript",
    )
    .await
    .unwrap();
    let prior = resume_prior_from_checkpoint(&fx.stores, &fx.session, &transcript)
        .await
        .unwrap();
    assert!(prior.is_none());
}

/// §38: a checkpoint whose cursor points beyond the durable log is never
/// trusted — resume falls back to full history instead.
#[tokio::test]
async fn a_cursor_beyond_the_log_fails_closed() {
    let fx = fixture().await;
    append_messages(&fx, &["hello"]).await;
    append_event(&fx, marker_event("e1")).await;
    // Forge an impossible cursor (the store records what it is given; the
    // committed-only guarantee is the WRITER's barrier discipline, so the
    // reader must still refuse to trust a cursor beyond the log).
    fx.stores
        .goal_checkpoints
        .create(
            leveler_storage::NewGoalCheckpoint {
                goal_id: fx.goal.clone(),
                session_id: fx.session.clone(),
                reason: CheckpointReason::Manual,
                event_cursor: 9_999,
                payload: leveler_lifecycle::GoalCheckpoint {
                    objective: "x".into(),
                    transcript_ordinal: Some(0),
                    ..Default::default()
                },
            },
            leveler_core::now(),
        )
        .await
        .unwrap();

    let transcript = leveler_engine::RawTranscript::load_strict(
        fx.stores.messages.as_ref(),
        &fx.session,
        "test transcript",
    )
    .await
    .unwrap();
    let prior = resume_prior_from_checkpoint(&fx.stores, &fx.session, &transcript)
        .await
        .unwrap();
    assert!(
        prior.is_none(),
        "an impossible cursor must fall back to full history, never be trusted"
    );
}

/// §29/§64: the restart reaper cuts a structured-only Interrupted checkpoint
/// for a goal left running by a dead process, and a repeated restart does not
/// duplicate it.
#[tokio::test]
async fn restart_reap_cuts_one_interrupted_checkpoint() {
    let fx = fixture().await;
    append_event(&fx, marker_event("work happened")).await;
    // A running turn left behind by the "dead" process.
    TurnRepository::new(&fx.db)
        .start(&fx.session, "user", None, leveler_core::now())
        .await
        .unwrap();

    let runtime = RuntimeId::new("runtime-restarted");
    reap_after_restart(&fx.stores, &runtime, None)
        .await
        .unwrap();

    let checkpoints = fx.stores.goal_checkpoints.for_goal(&fx.goal).await.unwrap();
    assert_eq!(checkpoints.len(), 1, "one interrupted checkpoint");
    assert_eq!(checkpoints[0].reason, CheckpointReason::Interrupted);
    // Structured-only: interruption never waits on model prose.
    assert!(checkpoints[0].payload.goal_summary.is_none());

    // The goal is still running (interrupted), so a second restart reaps
    // again — the same semantic boundary must not duplicate. Note the reap
    // itself appended a GoalCheckpointCreated event, so the SECOND pass sees
    // a new cursor only if new work happened; with none, dedupe must hold on
    // the new boundary too after one extra pass.
    let before = fx.stores.goal_checkpoints.for_goal(&fx.goal).await.unwrap();
    TurnRepository::new(&fx.db)
        .start(&fx.session, "user", None, leveler_core::now())
        .await
        .unwrap();
    reap_after_restart(&fx.stores, &runtime, None)
        .await
        .unwrap();
    reap_after_restart(&fx.stores, &runtime, None)
        .await
        .unwrap();
    let after = fx.stores.goal_checkpoints.for_goal(&fx.goal).await.unwrap();
    // The announcement event advances the log, so at most one additional
    // checkpoint may exist for the new boundary — never one per reap call.
    assert!(
        after.len() <= before.len() + 1,
        "repeated reaps must collapse: {} -> {}",
        before.len(),
        after.len()
    );
}

/// HCH-FIX-1 (§9C): a checkpoint created against a pre-/compact transcript
/// must never be eligible for post-/compact resume reconstruction — even
/// after the NEW conversation grows past the old transcript watermark,
/// where the ordinal guard alone would stop rejecting it.
#[tokio::test]
async fn a_pre_compact_checkpoint_is_never_consumed_after_the_cut() {
    let fx = fixture().await;
    // Pre-compact epoch: 4 transcript messages, a checkpoint at ordinal 4.
    append_messages(&fx, &["m1", "m2", "m3", "m4"]).await;
    append_event(&fx, marker_event("work")).await;
    let old = create_goal_checkpoint(
        &fx.stores,
        &fx.session,
        CheckpointReason::Manual,
        None,
        None,
    )
    .await
    .unwrap()
    .expect("pre-compact checkpoint");
    assert_eq!(old.payload.transcript_ordinal, Some(4));

    // The user compacts: one atomic cut replaces the transcript, appends the
    // epoch events, and invalidates the destroyed epoch's checkpoints.
    let summary = serde_json::to_string(&Message::text(Role::User, "compaction summary")).unwrap();
    let epoch_events: Vec<(String, String)> = [
        leveler_engine::EngineEvent::ContextSnapshot {
            messages: vec![Message::text(Role::User, "compaction summary")],
            through_ordinal: Some(1),
        },
        leveler_engine::EngineEvent::PlanUpdated { steps: vec![] },
    ]
    .into_iter()
    .map(|e| e.to_row().unwrap())
    .collect();
    fx.db
        .cut_context_epoch(&fx.session, &[summary], &epoch_events, leveler_core::now())
        .await
        .unwrap();

    // The new conversation grows PAST the old watermark (5 > 4): the ordinal
    // guard alone would no longer reject the stale checkpoint.
    append_messages(&fx, &["n1", "n2", "n3", "n4"]).await;
    let transcript = leveler_engine::RawTranscript::load_strict(
        fx.stores.messages.as_ref(),
        &fx.session,
        "post-compact transcript",
    )
    .await
    .unwrap();
    assert!(
        transcript.messages.len() > 4,
        "grown past the old watermark"
    );

    let prior = resume_prior_from_checkpoint(&fx.stores, &fx.session, &transcript)
        .await
        .unwrap();
    assert!(
        prior.is_none(),
        "an old-epoch checkpoint must never splice into a new-epoch delta"
    );
    assert!(
        fx.stores
            .goal_checkpoints
            .for_goal(&fx.goal)
            .await
            .unwrap()
            .is_empty(),
        "the cut invalidated the destroyed epoch's checkpoints"
    );
}
