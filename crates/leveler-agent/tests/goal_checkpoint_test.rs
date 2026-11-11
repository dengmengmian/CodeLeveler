//! Long-goal P3 contract tests: cursor exactness, checkpoint+delta resume,
//! idempotent triggers, corrupt-checkpoint fail-closed, and the interruption
//! trigger — all against real SQLite (in-memory), never mocked stores.

use leveler_agent::coding::{
    GoalCheckpointScope, create_goal_checkpoint, resume_prior_from_checkpoint,
};
use leveler_core::{ClarificationId, OwnerEpoch, RuntimeId, SessionId};
use leveler_engine::{EngineEvent, ReapScope, reap_after_restart};
use leveler_lifecycle::CheckpointReason;
use leveler_model::{Message, Role};
use leveler_storage::{
    Database, EngineStores, GoalStore, MessageRepository, OwnershipStore, SessionRecord,
    SessionRepository, TaskStore, TurnRepository,
};

struct Fixture {
    db: Database,
    stores: EngineStores,
    engine: leveler_engine::TaskEngine,
    session: SessionId,
    goal: leveler_core::GoalId,
    scope: GoalCheckpointScope,
    token: leveler_core::OwnershipToken,
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
    let token = db
        .acquire(
            &task,
            &RuntimeId::new("checkpoint-fixture"),
            &leveler_core::BootId::new("test-boot"),
            OwnerEpoch::new(0),
        )
        .await
        .unwrap();
    let goal = GoalStore::open(&db, &token, "port the parser", leveler_core::now())
        .await
        .unwrap();
    let root_payload = serde_json::json!({
        "version": 2,
        "objective": leveler_lifecycle::ObjectiveAnchor::from_session_goal("port the parser"),
        "goal_id": goal,
    })
    .to_string();
    let root = TurnRepository::new(&db)
        .start(&session, "user", Some(&root_payload), leveler_core::now())
        .await
        .unwrap();
    TurnRepository::new(&db)
        .finish(
            &leveler_core::TurnId::new(root.id.clone()),
            "completed",
            leveler_core::now(),
        )
        .await
        .unwrap();
    let scope = GoalCheckpointScope::new(
        goal.clone(),
        leveler_core::TurnId::new(&root.id),
        vec![leveler_core::TurnId::new(root.id)],
    );
    let stores = EngineStores::from_database(&db);
    let engine = leveler_engine::TaskEngine {
        stores: stores.clone(),
        runtime_id: RuntimeId::new("checkpoint-fixture"),
        boot: leveler_engine::EngineBoot {
            id: leveler_core::BootId::generate(),
            liveness: std::sync::Arc::new(leveler_test_support::TestBoots::new()),
        },
    };
    Fixture {
        db,
        stores,
        engine,
        session,
        goal,
        scope,
        token,
    }
}

async fn append_event(fx: &Fixture, event: EngineEvent) -> i64 {
    let (event_type, payload) = event.to_row().unwrap();
    leveler_storage::EventStore::append(
        &fx.db,
        &fx.session,
        Some(&fx.scope.root_turn_id),
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

async fn checkpoint_reaped(fx: &Fixture, outcome: &leveler_engine::ReapOutcome) {
    for reaped in &outcome.reaped_sessions {
        let Some(record) = create_goal_checkpoint(
            &fx.engine,
            &reaped.session_id,
            &fx.scope,
            CheckpointReason::Interrupted,
            None,
            None,
        )
        .await
        .unwrap() else {
            continue;
        };
        let log = leveler_engine::EventLog::new_owned(
            fx.stores.events.as_ref(),
            reaped.session_id.clone(),
            reaped.token.clone(),
        );
        log.append(
            None,
            leveler_agent::coding::checkpoint_created_event(&record),
            &mut |_| {},
        )
        .await
        .unwrap();
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
        &fx.engine,
        &fx.session,
        &fx.scope,
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
    let prior = resume_prior_from_checkpoint(&fx.stores, &fx.session, &fx.scope, &transcript)
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
        &fx.engine,
        &fx.session,
        &fx.scope,
        CheckpointReason::Manual,
        None,
        None,
    )
    .await
    .unwrap()
    .unwrap();
    let repeat = create_goal_checkpoint(
        &fx.engine,
        &fx.session,
        &fx.scope,
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
        &fx.engine,
        &fx.session,
        &fx.scope,
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
    let prior = resume_prior_from_checkpoint(&fx.stores, &fx.session, &fx.scope, &transcript)
        .await
        .unwrap();
    assert!(prior.is_none());
}

#[tokio::test]
async fn latest_scope_is_resolved_from_v2_turn_identity() {
    let fx = fixture().await;
    let resolved =
        leveler_agent::coding::latest_goal_checkpoint_scope(&fx.stores, &fx.session, &fx.goal)
            .await
            .unwrap()
            .expect("v2 goal turn has an exact scope");

    assert_eq!(resolved.goal_id, fx.goal);
    assert_eq!(resolved.root_turn_id, fx.scope.root_turn_id);
    assert_eq!(resolved.turn_ids, fx.scope.turn_ids);
}

#[tokio::test]
async fn latest_session_scope_does_not_fall_back_past_a_chat() {
    let fx = fixture().await;
    let chat_payload = serde_json::json!({
        "version": 2,
        "objective": leveler_lifecycle::ObjectiveAnchor::from_user_message("configure Ark"),
    })
    .to_string();
    TurnRepository::new(&fx.db)
        .start(
            &fx.session,
            "chat",
            Some(&chat_payload),
            leveler_core::now(),
        )
        .await
        .unwrap();

    assert!(
        leveler_agent::coding::latest_session_goal_checkpoint_scope(&fx.stores, &fx.session)
            .await
            .unwrap()
            .is_none(),
        "a current Chat must not install or create the older Goal's checkpoint"
    );
}

/// Checkpoint identity includes the continuation root, not only the goal.
/// A later, unrelated lineage of the same goal must take the full-history
/// path instead of installing stale plan/evidence context.
#[tokio::test]
async fn a_checkpoint_is_not_consumed_by_another_lineage_of_the_same_goal() {
    let fx = fixture().await;
    append_event(&fx, marker_event("old lineage work")).await;
    create_goal_checkpoint(
        &fx.engine,
        &fx.session,
        &fx.scope,
        CheckpointReason::Manual,
        None,
        None,
    )
    .await
    .unwrap()
    .expect("checkpoint");

    let other_scope = GoalCheckpointScope::new(
        fx.goal.clone(),
        leveler_core::TurnId::new("other-root"),
        vec![leveler_core::TurnId::new("other-root")],
    );
    let transcript = leveler_engine::RawTranscript::load_strict(
        fx.stores.messages.as_ref(),
        &fx.session,
        "test transcript",
    )
    .await
    .unwrap();

    assert!(
        resume_prior_from_checkpoint(&fx.stores, &fx.session, &other_scope, &transcript)
            .await
            .unwrap()
            .is_none()
    );
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
                    lineage_root_turn_id: Some(fx.scope.root_turn_id.as_str().to_string()),
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
    let prior = resume_prior_from_checkpoint(&fx.stores, &fx.session, &fx.scope, &transcript)
        .await
        .unwrap();
    assert!(
        prior.is_none(),
        "an impossible cursor must fall back to full history, never be trusted"
    );
}

/// §29/§64: Engine reports a mechanical reap boundary; Coding decides that it
/// means an Interrupted goal checkpoint.
#[tokio::test]
async fn coding_cuts_one_checkpoint_for_a_reaped_session() {
    let fx = fixture().await;
    append_event(&fx, marker_event("work happened")).await;
    // A running turn left behind by the "dead" process: a boot of the same
    // runtime that has since ended.
    let dead = leveler_engine::TaskEngine {
        stores: fx.stores.clone(),
        runtime_id: RuntimeId::new("checkpoint-fixture"),
        boot: leveler_engine::EngineBoot {
            id: leveler_core::BootId::generate(),
            liveness: std::sync::Arc::new(leveler_test_support::TestBoots::new()),
        },
    };
    let token = dead.acquire_ownership(&fx.session).await.unwrap();
    fx.stores
        .turns
        .start_owned(&token, &fx.session, "user", None, leveler_core::now())
        .await
        .unwrap();

    let outcome = reap_after_restart(&fx.engine, None, ReapScope::EndedBoots)
        .await
        .unwrap();
    assert_eq!(outcome.reaped_sessions.len(), 1);
    assert!(
        fx.stores
            .goal_checkpoints
            .for_goal(&fx.goal)
            .await
            .unwrap()
            .is_empty(),
        "the generic reaper must not create a Coding checkpoint"
    );
    checkpoint_reaped(&fx, &outcome).await;

    let checkpoints = fx.stores.goal_checkpoints.for_goal(&fx.goal).await.unwrap();
    assert_eq!(checkpoints.len(), 1, "one interrupted checkpoint");
    assert_eq!(checkpoints[0].reason, CheckpointReason::Interrupted);
    // Structured-only: interruption never waits on model prose.
    assert!(checkpoints[0].payload.goal_summary.is_none());
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
        &fx.engine,
        &fx.session,
        &fx.scope,
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
        leveler_engine::EngineEvent::PlanUpdated {
            steps: vec![],
            created: false,
            call_id: None,
        },
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

    let prior = resume_prior_from_checkpoint(&fx.stores, &fx.session, &fx.scope, &transcript)
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

// ── Cross-window continuity (Goal Continuity P1) ──────────────────────────

/// A user decision is a durable event fact, not something a summary may
/// paraphrase away. Window 2 must be able to read the question it never saw
/// and the answer the previous window received, even though the checkpoint
/// watermark excludes that conversation from the delta.
#[tokio::test]
async fn resume_after_yield_carries_the_answered_user_decision() {
    let fx = fixture().await;
    append_messages(&fx, &["window-1 user", "window-1 tool result"]).await;
    append_clarification(
        &fx,
        "req-refund-policy",
        "退款收入冲减政策？",
        &["冲减", "不冲减"],
        "按签约月份冲减，并保留审计日志",
        "answered",
    )
    .await;
    append_event(&fx, marker_event("first window work")).await;

    let record = create_goal_checkpoint(
        &fx.engine,
        &fx.session,
        &fx.scope,
        CheckpointReason::Milestone,
        None,
        None,
    )
    .await
    .unwrap()
    .expect("running goal checkpoint");
    assert_eq!(record.payload.transcript_ordinal, Some(2));

    // The next window starts after the watermark: its transcript delta is only
    // its own first message, so the decision has to travel in the checkpoint.
    append_messages(&fx, &["window-2 first message"]).await;
    let transcript = leveler_engine::RawTranscript::load_strict(
        fx.stores.messages.as_ref(),
        &fx.session,
        "test transcript",
    )
    .await
    .unwrap();
    let prior = resume_prior_from_checkpoint(&fx.stores, &fx.session, &fx.scope, &transcript)
        .await
        .unwrap()
        .expect("checkpoint resume");
    let texts = message_texts(&prior);
    let block = &texts[0];

    assert!(
        block.contains("退款收入冲减政策？"),
        "the question the previous window asked must survive:\n{block}"
    );
    assert!(
        block.contains("按签约月份冲减，并保留审计日志"),
        "the answer the user actually submitted must survive:\n{block}"
    );
    assert!(
        block.contains("req-refund-policy"),
        "the decision must be traceable to its durable request id:\n{block}"
    );
    assert_eq!(
        &texts[1..],
        &["window-2 first message".to_string()],
        "the checkpoint still owns everything before its watermark"
    );
}

/// Three windows: the newest checkpoint must carry the decisions and
/// runtime-observed execution facts of EVERY earlier window, not just the one
/// immediately before it.
#[tokio::test]
async fn three_windows_keep_every_decision_and_execution_fact() {
    let fx = fixture().await;
    // Window 1.
    append_messages(&fx, &["w1-user", "w1-tool"]).await;
    append_clarification(
        &fx,
        "req-1",
        "第一窗口决策",
        &["x", "y"],
        "选择甲方案并保留审计日志",
        "answered",
    )
    .await;
    let first = create_goal_checkpoint(
        &fx.engine,
        &fx.session,
        &fx.scope,
        CheckpointReason::Milestone,
        None,
        None,
    )
    .await
    .unwrap()
    .expect("first checkpoint");
    assert_eq!(first.payload.transcript_ordinal, Some(2));

    // Window 2: a new decision and a passing verification command.
    append_messages(&fx, &["w2-user", "w2-tool"]).await;
    append_tool_finished(
        &fx,
        "run_command",
        false,
        Some(0),
        "all tests passed: 42/42",
    )
    .await;
    append_clarification(
        &fx,
        "req-2",
        "第二窗口决策",
        &[],
        "继续使用 PostgreSQL 16",
        "answered",
    )
    .await;
    let second = create_goal_checkpoint(
        &fx.engine,
        &fx.session,
        &fx.scope,
        CheckpointReason::Milestone,
        None,
        None,
    )
    .await
    .unwrap()
    .expect("second checkpoint");
    assert_eq!(second.payload.transcript_ordinal, Some(4));

    // Window 3 resumes from the newest checkpoint.
    append_messages(&fx, &["w3-start"]).await;
    let transcript = leveler_engine::RawTranscript::load_strict(
        fx.stores.messages.as_ref(),
        &fx.session,
        "test transcript",
    )
    .await
    .unwrap();
    let prior = resume_prior_from_checkpoint(&fx.stores, &fx.session, &fx.scope, &transcript)
        .await
        .unwrap()
        .expect("checkpoint resume");
    let texts = message_texts(&prior);
    let block = &texts[0];

    assert!(
        block.contains("选择甲方案并保留审计日志"),
        "window 1's decision must still be present in window 3:\n{block}"
    );
    assert!(
        block.contains("继续使用 PostgreSQL 16"),
        "window 2's decision must be present in window 3:\n{block}"
    );
    assert!(
        block.contains("all tests passed: 42/42") && block.contains("exit 0"),
        "window 2's verified command outcome must be present in window 3:\n{block}"
    );
    assert_eq!(
        &texts[1..],
        &["w3-start".to_string()],
        "only the post-watermark delta is replayed"
    );
}

/// Compaction and yield share one projection. The latest checkpoint replaces
/// an older semantic summary, and the decision is not duplicated into the
/// recent tail.
#[tokio::test]
async fn compaction_keeps_decisions_and_a_newer_checkpoint_replaces_stale_summary() {
    let fx = fixture().await;
    append_messages(&fx, &["m0", "m1", "m2", "m3"]).await;
    append_clarification(
        &fx,
        "req-a",
        "账务口径",
        &[],
        "以权责发生制为准",
        "answered",
    )
    .await;
    let stale = create_goal_checkpoint(
        &fx.engine,
        &fx.session,
        &fx.scope,
        CheckpointReason::ContextCompaction,
        None,
        Some(leveler_agent::coding::SemanticRecap {
            goal_summary: Some("STALE: the user has not chosen yet".to_string()),
            ..Default::default()
        }),
    )
    .await
    .unwrap()
    .expect("compaction checkpoint");
    assert_eq!(stale.payload.transcript_ordinal, Some(4));

    // The work continues: a newer decision and message cross the next boundary.
    append_clarification(&fx, "req-b", "币种", &[], "以人民币入账", "answered").await;
    append_messages(&fx, &["after-compaction"]).await;
    let latest = create_goal_checkpoint(
        &fx.engine,
        &fx.session,
        &fx.scope,
        CheckpointReason::Milestone,
        None,
        None,
    )
    .await
    .unwrap()
    .expect("milestone checkpoint");
    assert_eq!(latest.payload.transcript_ordinal, Some(5));

    append_messages(&fx, &["tail-after-latest"]).await;
    let transcript = leveler_engine::RawTranscript::load_strict(
        fx.stores.messages.as_ref(),
        &fx.session,
        "test transcript",
    )
    .await
    .unwrap();
    let prior = resume_prior_from_checkpoint(&fx.stores, &fx.session, &fx.scope, &transcript)
        .await
        .unwrap()
        .expect("checkpoint resume");
    let texts = message_texts(&prior);
    let block = &texts[0];

    assert!(
        block.contains("以权责发生制为准") && block.contains("以人民币入账"),
        "both decisions survive compaction and a newer checkpoint:\n{block}"
    );
    assert!(
        !block.contains("STALE: the user has not chosen yet"),
        "the newer checkpoint must replace the stale summary:\n{block}"
    );
    assert_eq!(
        block.matches("以权责发生制为准").count(),
        1,
        "a decision must be rendered once, not duplicated:\n{block}"
    );
    assert_eq!(
        &texts[1..],
        &["tail-after-latest".to_string()],
        "only the post-watermark tail is replayed"
    );
}

/// A later user instruction is appended, never rewritten: the window sees the
/// original decision and the correction, in the order the person made them.
#[tokio::test]
async fn later_user_instructions_are_carried_in_order() {
    let fx = fixture().await;
    fx.db
        .append_amendment(&fx.token, &fx.goal, "先按方案 A 实现")
        .await
        .unwrap();
    fx.db
        .append_amendment(&fx.token, &fx.goal, "改为方案 B，A 作废")
        .await
        .unwrap();
    append_messages(&fx, &["work"]).await;

    create_goal_checkpoint(
        &fx.engine,
        &fx.session,
        &fx.scope,
        CheckpointReason::Milestone,
        None,
        None,
    )
    .await
    .unwrap()
    .expect("checkpoint");

    let transcript = leveler_engine::RawTranscript::load_strict(
        fx.stores.messages.as_ref(),
        &fx.session,
        "test transcript",
    )
    .await
    .unwrap();
    let prior = resume_prior_from_checkpoint(&fx.stores, &fx.session, &fx.scope, &transcript)
        .await
        .unwrap()
        .expect("checkpoint resume");
    let block = &message_texts(&prior)[0];

    let first = block.find("先按方案 A 实现").expect("original instruction");
    let second = block
        .find("改为方案 B，A 作废")
        .expect("latest instruction");
    assert!(
        first < second,
        "instructions must render in their durable order:\n{block}"
    );
}

/// Boundary truth: a pending request is not a decision, a skipped request is
/// not an approval, and a machine-ended request carries no user intent.
#[tokio::test]
async fn pending_skipped_and_cancelled_requests_are_not_fabricated_decisions() {
    let fx = fixture().await;
    append_messages(&fx, &["work"]).await;
    // Pending: requested, never answered.
    append_event(
        &fx,
        EngineEvent::ClarificationRequested {
            id: ClarificationId::new("req-pending"),
            question: "尚未回答的问题".into(),
            options: Vec::new(),
            call_id: "call-pending".into(),
            tool: "request_user_input".into(),
            questions: Vec::new(),
        },
    )
    .await;
    // Skipped: a person saw it and declined to answer.
    append_clarification(&fx, "req-skip", "被跳过的问题", &[], "", "skipped").await;
    // Cancelled by task cancellation: no person spoke.
    append_clarification(&fx, "req-cancel", "被取消的问题", &[], "", "cancelled").await;

    create_goal_checkpoint(
        &fx.engine,
        &fx.session,
        &fx.scope,
        CheckpointReason::Milestone,
        None,
        None,
    )
    .await
    .unwrap()
    .expect("checkpoint");

    let transcript = leveler_engine::RawTranscript::load_strict(
        fx.stores.messages.as_ref(),
        &fx.session,
        "test transcript",
    )
    .await
    .unwrap();
    let prior = resume_prior_from_checkpoint(&fx.stores, &fx.session, &fx.scope, &transcript)
        .await
        .unwrap()
        .expect("checkpoint resume");
    let block = &message_texts(&prior)[0];

    assert!(
        !block.contains("尚未回答的问题"),
        "a pending request must never be presented as a decision:\n{block}"
    );
    assert!(
        block.contains("被跳过的问题") && block.contains("skipped"),
        "a skipped question is a user action and must be explicit:\n{block}"
    );
    assert!(
        block.to_lowercase().contains("not an approval"),
        "a skip must never read as approval:\n{block}"
    );
    assert!(
        !block.contains("被取消的问题"),
        "a cancelled request carries no user decision:\n{block}"
    );
}

/// Only the terminal event's first answer counts, and repeated resumes return
/// the same context. A crash between the answer and the next window must not
/// let a duplicate late answer replace the recorded one.
#[tokio::test]
async fn decisions_are_idempotent_and_first_terminal_wins() {
    let fx = fixture().await;
    append_messages(&fx, &["work"]).await;
    append_clarification(&fx, "req-once", "只能答一次", &[], "第一次答案", "answered").await;
    // A duplicate terminal row is history the writer normally prevents; the
    // projection must still keep the first decision.
    append_event(
        &fx,
        EngineEvent::ClarificationAnswered {
            id: ClarificationId::new("req-once"),
            answer: "第二次答案".into(),
            outcome: "answered".into(),
        },
    )
    .await;

    create_goal_checkpoint(
        &fx.engine,
        &fx.session,
        &fx.scope,
        CheckpointReason::Milestone,
        None,
        None,
    )
    .await
    .unwrap()
    .expect("checkpoint");

    let transcript = leveler_engine::RawTranscript::load_strict(
        fx.stores.messages.as_ref(),
        &fx.session,
        "test transcript",
    )
    .await
    .unwrap();
    let first = resume_prior_from_checkpoint(&fx.stores, &fx.session, &fx.scope, &transcript)
        .await
        .unwrap()
        .expect("checkpoint resume");
    let second = resume_prior_from_checkpoint(&fx.stores, &fx.session, &fx.scope, &transcript)
        .await
        .unwrap()
        .expect("checkpoint resume");
    assert_eq!(
        message_texts(&first),
        message_texts(&second),
        "resume is pure"
    );

    let block = &message_texts(&first)[0];
    assert!(block.contains("第一次答案"), "{block}");
    assert!(
        !block.contains("第二次答案"),
        "the first terminal wins, exactly like the writer:\n{block}"
    );
}

/// A decision recorded against another goal's turn is not this lineage's fact.
#[tokio::test]
async fn decisions_from_another_lineage_are_not_carried() {
    let fx = fixture().await;
    append_messages(&fx, &["work"]).await;
    let foreign = TurnRepository::new(&fx.db)
        .start(&fx.session, "user", None, leveler_core::now())
        .await
        .unwrap();
    let foreign_turn = leveler_core::TurnId::new(foreign.id);
    for event in [
        EngineEvent::ClarificationRequested {
            id: ClarificationId::new("req-foreign"),
            question: "别的目标的决策".into(),
            options: Vec::new(),
            call_id: "call-foreign".into(),
            tool: "request_user_input".into(),
            questions: Vec::new(),
        },
        EngineEvent::ClarificationAnswered {
            id: ClarificationId::new("req-foreign"),
            answer: "别的目标答案".into(),
            outcome: "answered".into(),
        },
    ] {
        let (event_type, payload) = event.to_row().unwrap();
        leveler_storage::EventStore::append(
            &fx.db,
            &fx.session,
            Some(&foreign_turn),
            &event_type,
            &payload,
            leveler_core::now(),
        )
        .await
        .unwrap();
    }

    create_goal_checkpoint(
        &fx.engine,
        &fx.session,
        &fx.scope,
        CheckpointReason::Milestone,
        None,
        None,
    )
    .await
    .unwrap()
    .expect("checkpoint");
    let transcript = leveler_engine::RawTranscript::load_strict(
        fx.stores.messages.as_ref(),
        &fx.session,
        "test transcript",
    )
    .await
    .unwrap();
    let prior = resume_prior_from_checkpoint(&fx.stores, &fx.session, &fx.scope, &transcript)
        .await
        .unwrap()
        .expect("checkpoint resume");
    let block = &message_texts(&prior)[0];
    assert!(
        !block.contains("别的目标的决策") && !block.contains("别的目标答案"),
        "another lineage's decision must not leak into this goal:\n{block}"
    );
}

/// Runtime-observed outcomes cross the boundary: commands carry their exit
/// code and failures their reason, while non-actionable successes (a read) are
/// not promoted into evidence.
#[tokio::test]
async fn runtime_observed_execution_facts_cross_the_boundary() {
    let fx = fixture().await;
    append_messages(&fx, &["work"]).await;
    append_tool_finished(
        &fx,
        "run_command",
        true,
        Some(1),
        "cargo test failed: 1 failing",
    )
    .await;
    append_tool_finished(&fx, "read_file", false, None, "ordinary file contents").await;

    create_goal_checkpoint(
        &fx.engine,
        &fx.session,
        &fx.scope,
        CheckpointReason::Milestone,
        None,
        None,
    )
    .await
    .unwrap()
    .expect("checkpoint");
    let transcript = leveler_engine::RawTranscript::load_strict(
        fx.stores.messages.as_ref(),
        &fx.session,
        "test transcript",
    )
    .await
    .unwrap();
    let prior = resume_prior_from_checkpoint(&fx.stores, &fx.session, &fx.scope, &transcript)
        .await
        .unwrap()
        .expect("checkpoint resume");
    let block = &message_texts(&prior)[0];

    assert!(
        block.contains("run_command") && block.contains("cargo test failed: 1 failing"),
        "a failed verification must survive the window boundary:\n{block}"
    );
    assert!(
        block.contains("exit 1"),
        "the runtime-observed exit code must be carried:\n{block}"
    );
    assert!(
        !block.contains("ordinary file contents"),
        "a successful read is not execution evidence:\n{block}"
    );
}

async fn append_clarification(
    fx: &Fixture,
    id: &str,
    question: &str,
    options: &[&str],
    answer: &str,
    outcome: &str,
) {
    append_event(
        fx,
        EngineEvent::ClarificationRequested {
            id: ClarificationId::new(id),
            question: question.to_string(),
            options: options.iter().map(|value| value.to_string()).collect(),
            call_id: format!("call-{id}"),
            tool: "request_user_input".into(),
            questions: Vec::new(),
        },
    )
    .await;
    append_event(
        fx,
        EngineEvent::ClarificationAnswered {
            id: ClarificationId::new(id),
            answer: answer.to_string(),
            outcome: outcome.to_string(),
        },
    )
    .await;
}

async fn append_tool_finished(
    fx: &Fixture,
    name: &str,
    is_error: bool,
    exit_code: Option<i32>,
    preview: &str,
) {
    append_event(
        fx,
        EngineEvent::ToolCallFinished {
            call_id: format!("call-{name}-{exit_code:?}-{}", preview.len()),
            name: name.to_string(),
            is_error,
            preview: preview.to_string(),
            agent_id: None,
            applied_diff: None,
            exit_code,
            stop: None,
        },
    )
    .await;
}

/// A person's mid-window correction is durable user input, not model summary:
/// the exact submitted text reaches the next window even though the
/// checkpoint watermark excludes its transcript row. Runtime notices are
/// never presented as the person speaking.
#[tokio::test]
async fn submitted_user_messages_cross_the_boundary_in_order() {
    let fx = fixture().await;
    let user_payloads: Vec<String> = ["先按方案 A 实现", "不要动迁移脚本"]
        .iter()
        .map(|text| serde_json::to_string(&Message::user_input(*text)).unwrap())
        .collect();
    MessageRepository::new(&fx.db)
        .append_in_turn(
            &fx.session,
            &fx.scope.root_turn_id,
            &user_payloads,
            leveler_core::now(),
        )
        .await
        .unwrap();
    let notice = serde_json::to_string(&Message::user(
        "runtime notice text",
        leveler_model::TranscriptOrigin::RuntimeNotice {
            notice: leveler_model::RuntimeNoticeKind::GoalCheckpoint,
        },
    ))
    .unwrap();
    MessageRepository::new(&fx.db)
        .append_in_turn(
            &fx.session,
            &fx.scope.root_turn_id,
            &[notice],
            leveler_core::now(),
        )
        .await
        .unwrap();
    append_messages(&fx, &["window work"]).await;

    create_goal_checkpoint(
        &fx.engine,
        &fx.session,
        &fx.scope,
        CheckpointReason::Milestone,
        None,
        None,
    )
    .await
    .unwrap()
    .expect("checkpoint");
    let transcript = leveler_engine::RawTranscript::load_strict(
        fx.stores.messages.as_ref(),
        &fx.session,
        "test transcript",
    )
    .await
    .unwrap();
    let prior = resume_prior_from_checkpoint(&fx.stores, &fx.session, &fx.scope, &transcript)
        .await
        .unwrap()
        .expect("checkpoint resume");
    let block = &message_texts(&prior)[0];

    let first = block
        .find("先按方案 A 实现")
        .expect("first submitted message");
    let second = block
        .find("不要动迁移脚本")
        .expect("second submitted message");
    assert!(
        first < second,
        "submitted messages keep their order:\n{block}"
    );
    assert!(
        !block.contains("runtime notice text"),
        "harness notices must never read as user input:\n{block}"
    );
}
