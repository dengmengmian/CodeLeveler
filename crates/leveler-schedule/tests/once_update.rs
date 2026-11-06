//! Trigger-change regressions for persisted definitions.
//!
//! A definition update replaces the rule a schedule owns. The replacement slot
//! must therefore come from the new rule, and a one-off's only slot is its
//! declared instant. These tests use an isolated SQLite file and the store
//! directly, so no dispatcher or client can hide a missing occurrence.

use std::path::PathBuf;
use std::time::Duration;

use chrono::{DateTime, Utc};
use leveler_client_protocol::PermissionProfile;
use leveler_schedule::{
    Claim, DefinitionUpdate, MisfirePolicy, NewScheduleDefinition, OccurrenceState, OverlapPolicy,
    RunOutcome, ScheduleAction, ScheduleDefinition, ScheduleError, ScheduleId, ScheduleStore,
    ScheduleTrigger, SessionPolicy,
};

struct Fixture {
    store: ScheduleStore,
    workspace: PathBuf,
    _tmp: tempfile::TempDir,
}

async fn fixture() -> Fixture {
    let tmp = tempfile::tempdir().unwrap();
    let store = ScheduleStore::open(&tmp.path().join("schedules.db"))
        .await
        .unwrap();
    let workspace = tmp.path().join("workspace");
    std::fs::create_dir_all(&workspace).unwrap();
    Fixture {
        store,
        workspace,
        _tmp: tmp,
    }
}

/// A millisecond-aligned future instant. Stored timestamps keep milliseconds,
/// so a test value with nanoseconds would not round-trip and would compare
/// unequal after an update for reasons unrelated to the defect.
fn at_millis(offset_secs: i64) -> DateTime<Utc> {
    let now_ms = leveler_core::now().timestamp_millis();
    DateTime::from_timestamp_millis(now_ms + offset_secs * 1000).expect("timestamp")
}

fn once(at: DateTime<Utc>) -> ScheduleTrigger {
    ScheduleTrigger::Once { at }
}

async fn create(fixture: &Fixture, trigger: ScheduleTrigger) -> ScheduleDefinition {
    create_with_misfire(fixture, trigger, MisfirePolicy::Skip).await
}

async fn create_with_misfire(
    fixture: &Fixture,
    trigger: ScheduleTrigger,
    misfire: MisfirePolicy,
) -> ScheduleDefinition {
    fixture
        .store
        .create_definition(NewScheduleDefinition {
            name: "one-off".into(),
            workspace: fixture.workspace.clone(),
            action: ScheduleAction::Shell {
                command: "echo scheduled".into(),
            },
            trigger,
            session_policy: SessionPolicy::Continue,
            overlap: OverlapPolicy::SkipIfRunning,
            mode: PermissionProfile::FullAccess,
            model: None,
            timeout: Duration::from_secs(600),
            misfire,
            misfire_grace: Duration::from_secs(300),
            unattended: None,
        })
        .await
        .unwrap()
}

fn update(
    definition: &ScheduleDefinition,
    name: &str,
    trigger: ScheduleTrigger,
) -> DefinitionUpdate {
    DefinitionUpdate {
        name: name.into(),
        workspace: definition.workspace.clone(),
        action: definition.action.clone(),
        trigger,
        session_policy: definition.session_policy,
        overlap: definition.overlap,
        mode: definition.mode,
        model: definition.model.clone(),
        timeout: definition.timeout,
        misfire: definition.misfire,
        misfire_grace: definition.misfire_grace,
        grant: None,
    }
}

fn retime(definition: &ScheduleDefinition, at: DateTime<Utc>) -> DefinitionUpdate {
    update(definition, &definition.name, once(at))
}

async fn update_trigger(
    fixture: &Fixture,
    definition: &ScheduleDefinition,
    trigger: ScheduleTrigger,
) -> Result<ScheduleDefinition, ScheduleError> {
    fixture
        .store
        .update_definition(
            &definition.id,
            update(definition, &definition.name, trigger),
            None,
        )
        .await
}

async fn pending_slots(fixture: &Fixture, id: &ScheduleId) -> Vec<DateTime<Utc>> {
    fixture
        .store
        .occurrences(id, 100)
        .await
        .unwrap()
        .into_iter()
        .filter(|occurrence| occurrence.state == OccurrenceState::Pending)
        .map(|occurrence| occurrence.scheduled_for)
        .collect()
}

async fn states(fixture: &Fixture, id: &ScheduleId) -> Vec<OccurrenceState> {
    fixture
        .store
        .occurrences(id, 100)
        .await
        .unwrap()
        .into_iter()
        .map(|occurrence| occurrence.state)
        .collect()
}

async fn claim_due(fixture: &Fixture, id: &ScheduleId, now: DateTime<Utc>) -> Option<Claim> {
    fixture
        .store
        .claim_due(now, Duration::from_secs(60), 10)
        .await
        .unwrap()
        .into_iter()
        .find(|claim| &claim.definition.id == id)
}

async fn settle(
    fixture: &Fixture,
    id: &ScheduleId,
    now: DateTime<Utc>,
    outcome: RunOutcome,
) -> Claim {
    let claim = claim_due(fixture, id, now)
        .await
        .expect("the definition must have a due, claimable slot");
    fixture
        .store
        .finish_run(&claim.run_id, outcome)
        .await
        .unwrap()
        .expect("the definition still exists");
    claim
}

#[tokio::test]
async fn moving_a_future_one_off_moves_its_pending_occurrence() {
    let fixture = fixture().await;
    let t1 = at_millis(3_600);
    let definition = create(&fixture, once(t1)).await;

    let before = fixture.store.occurrences(&definition.id, 10).await.unwrap();
    assert_eq!(before.len(), 1, "create must generate the one-off's slot");
    assert_eq!(before[0].state, OccurrenceState::Pending);
    assert_eq!(before[0].scheduled_for, t1);

    let t2 = at_millis(7_200);
    let updated = fixture
        .store
        .update_definition(&definition.id, retime(&definition, t2), None)
        .await
        .unwrap();
    assert_eq!(updated.trigger, once(t2));

    assert_eq!(
        pending_slots(&fixture, &definition.id).await,
        vec![t2],
        "the new one-off must own exactly one pending slot at its new time"
    );
    let next = fixture
        .store
        .next_pending_occurrence(&definition.id)
        .await
        .unwrap()
        .expect("the schedule must still have a next run");
    assert_eq!(next.scheduled_for, t2);

    assert!(
        claim_due(&fixture, &definition.id, t1).await.is_none(),
        "the old time must not be claimable after the rewrite"
    );
    let claim = claim_due(&fixture, &definition.id, t2)
        .await
        .expect("the new time must be claimable");
    fixture
        .store
        .finish_run(&claim.run_id, RunOutcome::Succeeded { exit_code: Some(0) })
        .await
        .unwrap();
    assert!(
        pending_slots(&fixture, &definition.id).await.is_empty(),
        "a one-off must not generate a second slot after it runs"
    );
}

#[tokio::test]
async fn moving_a_one_off_earlier_keeps_it_pending_and_drops_the_old_slot() {
    let fixture = fixture().await;
    let t1 = at_millis(7_200);
    let definition = create(&fixture, once(t1)).await;

    let t2 = at_millis(1_800);
    assert!(t2 < t1);
    update_trigger(&fixture, &definition, once(t2))
        .await
        .unwrap();

    assert_eq!(pending_slots(&fixture, &definition.id).await, vec![t2]);
    assert_eq!(
        fixture
            .store
            .occurrences(&definition.id, 10)
            .await
            .unwrap()
            .len(),
        1,
        "the replaced pending slot is gone, not duplicated"
    );
    assert!(claim_due(&fixture, &definition.id, t2).await.is_some());
}

#[tokio::test]
async fn an_identical_trigger_update_does_not_duplicate_the_slot() {
    let fixture = fixture().await;
    let t1 = at_millis(3_600);
    let definition = create(&fixture, once(t1)).await;

    let updated = fixture
        .store
        .update_definition(
            &definition.id,
            update(&definition, &definition.name, definition.trigger.clone()),
            None,
        )
        .await
        .unwrap();

    assert_eq!(updated.trigger, once(t1));
    assert_eq!(pending_slots(&fixture, &definition.id).await, vec![t1]);
    assert_eq!(
        fixture
            .store
            .occurrences(&definition.id, 10)
            .await
            .unwrap()
            .len(),
        1
    );
}

#[tokio::test]
async fn updating_twice_before_the_slot_runs_leaves_only_the_last_time() {
    let fixture = fixture().await;
    let definition = create(&fixture, once(at_millis(3_600))).await;

    let t2 = at_millis(5_400);
    let definition = update_trigger(&fixture, &definition, once(t2))
        .await
        .unwrap();
    let t3 = at_millis(9_000);
    update_trigger(&fixture, &definition, once(t3))
        .await
        .unwrap();

    assert_eq!(pending_slots(&fixture, &definition.id).await, vec![t3]);
    assert_eq!(
        fixture
            .store
            .occurrences(&definition.id, 10)
            .await
            .unwrap()
            .len(),
        1,
        "pending slots generated by the intermediate rule must not accumulate"
    );
}

#[tokio::test]
async fn a_one_off_updated_close_to_the_past_is_still_claimable() {
    let fixture = fixture().await;
    let definition = create(&fixture, once(at_millis(3_600))).await;

    let past = at_millis(-60);
    update_trigger(&fixture, &definition, once(past))
        .await
        .unwrap();

    assert_eq!(pending_slots(&fixture, &definition.id).await, vec![past]);
    assert_eq!(
        fixture
            .store
            .reconcile_misfires(leveler_core::now())
            .await
            .unwrap(),
        0,
        "a slot within the grace window is due, not missed"
    );
    assert!(
        claim_due(&fixture, &definition.id, leveler_core::now())
            .await
            .is_some()
    );
}

#[tokio::test]
async fn a_one_off_updated_far_into_the_past_is_recorded_missed() {
    let fixture = fixture().await;
    let definition = create(&fixture, once(at_millis(3_600))).await;

    let past = at_millis(-3_600);
    update_trigger(&fixture, &definition, once(past))
        .await
        .unwrap();

    assert_eq!(
        fixture
            .store
            .reconcile_misfires(leveler_core::now())
            .await
            .unwrap(),
        1,
        "the skip policy must settle the overdue slot instead of leaving it silent"
    );
    assert_eq!(
        states(&fixture, &definition.id).await,
        vec![OccurrenceState::Missed]
    );
    assert!(
        claim_due(&fixture, &definition.id, leveler_core::now())
            .await
            .is_none()
    );
    assert!(
        fixture
            .store
            .next_pending_occurrence(&definition.id)
            .await
            .unwrap()
            .is_none()
    );
}

#[tokio::test]
async fn a_one_off_updated_far_into_the_past_runs_once_on_recovery() {
    let fixture = fixture().await;
    let definition = create_with_misfire(
        &fixture,
        once(at_millis(3_600)),
        MisfirePolicy::RunOnceOnRecovery,
    )
    .await;

    update_trigger(&fixture, &definition, once(at_millis(-3_600)))
        .await
        .unwrap();

    assert_eq!(
        fixture
            .store
            .reconcile_misfires(leveler_core::now())
            .await
            .unwrap(),
        0,
        "run-once recovery keeps the newest overdue slot pending"
    );
    assert!(
        claim_due(&fixture, &definition.id, leveler_core::now())
            .await
            .is_some()
    );
}

#[tokio::test]
async fn terminal_one_offs_are_not_revived_by_an_ordinary_edit() {
    let cases = [
        ("completed", RunOutcome::Succeeded { exit_code: Some(0) }),
        ("cancelled", RunOutcome::Cancelled { error: None }),
        (
            "blocked",
            RunOutcome::Blocked {
                error: "authorization refused".into(),
            },
        ),
        (
            "waiting for approval",
            RunOutcome::WaitingApproval {
                error: "no decision".into(),
            },
        ),
    ];

    for (label, outcome) in cases {
        let fixture = fixture().await;
        let t0 = at_millis(3_600);
        let definition = create(&fixture, once(t0)).await;
        settle(&fixture, &definition.id, t0, outcome).await;

        let after = fixture
            .store
            .update_definition(
                &definition.id,
                update(&definition, "renamed", definition.trigger.clone()),
                None,
            )
            .await
            .unwrap();

        assert_eq!(after.name, "renamed");
        assert!(
            pending_slots(&fixture, &definition.id).await.is_empty(),
            "{label}: an ordinary edit must not schedule a terminal one-off again"
        );
        assert_eq!(
            fixture
                .store
                .occurrences(&definition.id, 10)
                .await
                .unwrap()
                .len(),
            1,
            "{label}: history must not be rewritten"
        );
    }
}

#[tokio::test]
async fn a_terminal_one_off_can_be_explicitly_rescheduled() {
    let fixture = fixture().await;
    let t0 = at_millis(3_600);
    let definition = create(&fixture, once(t0)).await;
    settle(
        &fixture,
        &definition.id,
        t0,
        RunOutcome::Succeeded { exit_code: Some(0) },
    )
    .await;

    let t1 = at_millis(7_200);
    let updated = update_trigger(&fixture, &definition, once(t1))
        .await
        .unwrap();
    assert_eq!(
        pending_slots(&fixture, &definition.id).await,
        vec![t1],
        "an explicit trigger change reschedules a terminal one-off"
    );
    assert_eq!(
        fixture
            .store
            .occurrences(&definition.id, 10)
            .await
            .unwrap()
            .len(),
        2,
        "the completed occurrence remains part of history"
    );

    settle(
        &fixture,
        &updated.id,
        t1,
        RunOutcome::Succeeded { exit_code: Some(0) },
    )
    .await;
    assert!(
        pending_slots(&fixture, &definition.id).await.is_empty(),
        "the rescheduled one-off must not run a second time"
    );
    assert_eq!(
        fixture
            .store
            .occurrences(&definition.id, 10)
            .await
            .unwrap()
            .len(),
        2
    );
}

#[tokio::test]
async fn updating_while_a_one_off_runs_keeps_the_active_run_and_the_new_slot() {
    let fixture = fixture().await;
    let t0 = at_millis(3_600);
    let definition = create(&fixture, once(t0)).await;
    let active = claim_due(&fixture, &definition.id, t0)
        .await
        .expect("the original run must be active");

    let t1 = at_millis(7_200);
    let updated = update_trigger(&fixture, &definition, once(t1))
        .await
        .unwrap();

    assert_eq!(
        states(&fixture, &definition.id).await,
        // Occurrences read newest first: the new pending slot ahead of the
        // claimed original run.
        vec![OccurrenceState::Pending, OccurrenceState::Claimed]
    );
    assert!(
        claim_due(&fixture, &definition.id, t1).await.is_none(),
        "the overlap policy keeps the new slot pending while the run is active"
    );

    fixture
        .store
        .finish_run(&active.run_id, RunOutcome::Succeeded { exit_code: Some(0) })
        .await
        .unwrap();
    assert_eq!(
        pending_slots(&fixture, &updated.id).await,
        vec![t1],
        "the active run's settlement must not consume or duplicate the new slot"
    );
    assert!(claim_due(&fixture, &definition.id, t1).await.is_some());
}

#[tokio::test]
async fn rescheduling_onto_an_instant_with_history_is_refused_atomically() {
    let fixture = fixture().await;
    let t0 = at_millis(3_600);
    let definition = create(&fixture, once(t0)).await;
    settle(
        &fixture,
        &definition.id,
        t0,
        RunOutcome::Succeeded { exit_code: Some(0) },
    )
    .await;

    let t1 = at_millis(7_200);
    let updated = update_trigger(&fixture, &definition, once(t1))
        .await
        .unwrap();

    let error = fixture
        .store
        .update_definition(
            &definition.id,
            update(&updated, &updated.name, once(t0)),
            None,
        )
        .await
        .expect_err("an instant with terminal history cannot become a pending slot again");
    assert!(
        error
            .to_string()
            .contains("already exists in this schedule's history"),
        "unexpected error: {error}"
    );

    let reread = fixture
        .store
        .definition(&definition.id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        reread.trigger,
        once(t1),
        "the refused update must leave the definition unchanged"
    );
    assert_eq!(
        pending_slots(&fixture, &definition.id).await,
        vec![t1],
        "the refused update must not delete the slot it could not replace"
    );
}

#[tokio::test]
async fn two_editors_with_the_same_revision_do_not_overwrite_each_other() {
    let fixture = fixture().await;
    let definition = create(&fixture, once(at_millis(3_600))).await;
    let t2 = at_millis(5_400);
    let t3 = at_millis(9_000);

    let first = fixture.store.clone();
    let second = fixture.store.clone();
    let base = definition.clone();
    let (left, right) = tokio::join!(
        first.update_definition(&base.id, retime(&base, t2), Some(base.updated_at),),
        second.update_definition(&base.id, retime(&base, t3), Some(base.updated_at),),
    );

    let winner = match (left, right) {
        (Ok(winner), Err(ScheduleError::Conflict(_)))
        | (Err(ScheduleError::Conflict(_)), Ok(winner)) => winner,
        (left, right) => panic!("expected exactly one conflict, got {left:?} / {right:?}"),
    };
    let winning_time = match winner.trigger {
        ScheduleTrigger::Once { at } => at,
        other => panic!("unexpected trigger: {other:?}"),
    };
    assert_eq!(
        pending_slots(&fixture, &definition.id).await,
        vec![winning_time],
        "only the winning editor's slot may exist"
    );
    let reread = fixture
        .store
        .definition(&definition.id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(reread.trigger, winner.trigger);
}

#[tokio::test]
async fn a_one_off_updated_to_the_current_instant_is_claimable() {
    let fixture = fixture().await;
    let definition = create(&fixture, once(at_millis(3_600))).await;

    let now = at_millis(0);
    update_trigger(&fixture, &definition, once(now))
        .await
        .unwrap();
    assert_eq!(pending_slots(&fixture, &definition.id).await, vec![now]);

    let claim = claim_due(&fixture, &definition.id, leveler_core::now())
        .await
        .expect("a slot scheduled at the current instant is immediately due");
    assert_eq!(claim.occurrence.scheduled_for, now);
}
