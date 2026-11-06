//! Trigger-change regressions for the recurring rule families.
//!
//! `once_update.rs` pins the one-off side of the update path. These tests pin
//! the other side: re-declaring a daily, weekly, interval or cron rule must
//! replace the pending slot with the new rule's first future slot. The old
//! slot must disappear, no second slot may stack up, and a rule that offers no
//! future occurrence must fail the update instead of reporting a success that
//! cannot run.

use std::path::PathBuf;
use std::time::Duration;

use chrono::{DateTime, Datelike, Timelike, Utc};
use leveler_client_protocol::PermissionProfile;
use leveler_schedule::{
    DefinitionUpdate, LocalTime, MisfirePolicy, NewScheduleDefinition, OccurrenceState,
    OverlapPolicy, ScheduleAction, ScheduleDefinition, ScheduleError, ScheduleId, ScheduleStore,
    ScheduleTrigger, SessionPolicy, WeekdaySet, parse_timezone,
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

fn at_millis(offset_secs: i64) -> DateTime<Utc> {
    let now_ms = leveler_core::now().timestamp_millis();
    DateTime::from_timestamp_millis(now_ms + offset_secs * 1000).expect("timestamp")
}

fn daily(time: &str, zone: &str) -> ScheduleTrigger {
    ScheduleTrigger::Daily {
        time: LocalTime::parse(time).expect("time"),
        timezone: parse_timezone(zone).expect("zone"),
    }
}

fn weekly(weekdays: &str, time: &str, zone: &str) -> ScheduleTrigger {
    ScheduleTrigger::Weekly {
        weekdays: WeekdaySet::parse(weekdays).expect("weekdays"),
        time: LocalTime::parse(time).expect("time"),
        timezone: parse_timezone(zone).expect("zone"),
    }
}

fn every(anchor: DateTime<Utc>, every_secs: u64) -> ScheduleTrigger {
    ScheduleTrigger::Every {
        anchor,
        every: Duration::from_secs(every_secs),
    }
}

fn cron(expression: &str, zone: &str) -> ScheduleTrigger {
    ScheduleTrigger::Cron {
        expression: expression.into(),
        timezone: parse_timezone(zone).expect("zone"),
    }
}

async fn create(fixture: &Fixture, trigger: ScheduleTrigger) -> ScheduleDefinition {
    fixture
        .store
        .create_definition(NewScheduleDefinition {
            name: "recurring".into(),
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
            misfire: MisfirePolicy::Skip,
            misfire_grace: Duration::from_secs(300),
            unattended: None,
        })
        .await
        .unwrap()
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
            DefinitionUpdate {
                name: definition.name.clone(),
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
            },
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

async fn only_pending(fixture: &Fixture, id: &ScheduleId) -> DateTime<Utc> {
    let slots = pending_slots(fixture, id).await;
    assert_eq!(
        slots.len(),
        1,
        "a recurring definition must own exactly one pending slot, got {slots:?}"
    );
    slots[0]
}

#[tokio::test]
async fn updating_a_daily_time_replaces_the_pending_slot() {
    let fixture = fixture().await;
    let definition = create(&fixture, daily("09:00", "Asia/Shanghai")).await;
    let before = only_pending(&fixture, &definition.id).await;

    let updated = update_trigger(&fixture, &definition, daily("10:15", "Asia/Shanghai"))
        .await
        .unwrap();

    let after = only_pending(&fixture, &definition.id).await;
    assert_ne!(after, before, "the old pending slot must be replaced");
    let zone = parse_timezone("Asia/Shanghai").unwrap();
    let local = after.with_timezone(&zone);
    assert_eq!((local.hour(), local.minute()), (10, 15));
    let next = fixture
        .store
        .next_pending_occurrence(&definition.id)
        .await
        .unwrap()
        .expect("the schedule must still have a next run");
    assert_eq!(next.scheduled_for, after);
    assert_eq!(updated.trigger, daily("10:15", "Asia/Shanghai"));
}

#[tokio::test]
async fn updating_weekly_days_and_time_replaces_the_pending_slot() {
    let fixture = fixture().await;
    let definition = create(&fixture, weekly("mon", "09:00", "UTC")).await;
    let before = only_pending(&fixture, &definition.id).await;

    update_trigger(&fixture, &definition, weekly("wed", "10:15", "UTC"))
        .await
        .unwrap();

    let after = only_pending(&fixture, &definition.id).await;
    assert_ne!(after, before, "the old pending slot must be replaced");
    let local = after.with_timezone(&parse_timezone("UTC").unwrap());
    assert_eq!(local.weekday(), chrono::Weekday::Wed);
    assert_eq!((local.hour(), local.minute()), (10, 15));
}

#[tokio::test]
async fn updating_an_interval_uses_the_new_anchor_and_interval() {
    let fixture = fixture().await;
    let anchor = at_millis(3_600);
    let definition = create(&fixture, every(anchor, 3_600)).await;
    assert_eq!(only_pending(&fixture, &definition.id).await, anchor);

    let later = at_millis(7_200);
    let updated = update_trigger(&fixture, &definition, every(later, 1_800))
        .await
        .unwrap();

    assert_eq!(
        only_pending(&fixture, &definition.id).await,
        later,
        "the new anchor is the only slot the new rule offers yet"
    );
    assert_eq!(updated.trigger, every(later, 1_800));
}

#[tokio::test]
async fn updating_an_interval_does_not_reactivate_its_old_anchor() {
    let fixture = fixture().await;
    let definition = create(&fixture, every(at_millis(3_600), 3_600)).await;

    // The new declaration names an anchor in the past. An update advances to
    // the first future grid point; it must not resurrect the anchor as an
    // immediate run the way creation of an interval would.
    let past_anchor = at_millis(-7_200);
    update_trigger(&fixture, &definition, every(past_anchor, 3_600))
        .await
        .unwrap();

    let slot = only_pending(&fixture, &definition.id).await;
    assert!(
        slot > leveler_core::now(),
        "an update must leave a future slot, got {slot}"
    );
    assert_eq!(
        (slot - past_anchor).num_seconds().rem_euclid(3_600),
        0,
        "the replacement slot must lie on the new interval grid"
    );
}

#[tokio::test]
async fn updating_a_cron_expression_replaces_the_pending_slot() {
    let fixture = fixture().await;
    let definition = create(&fixture, cron("0 9 * * *", "UTC")).await;
    let before = only_pending(&fixture, &definition.id).await;

    update_trigger(&fixture, &definition, cron("30 10 * * *", "UTC"))
        .await
        .unwrap();

    let after = only_pending(&fixture, &definition.id).await;
    assert_ne!(after, before, "the old pending slot must be replaced");
    let local = after.with_timezone(&parse_timezone("UTC").unwrap());
    assert_eq!((local.hour(), local.minute()), (10, 30));
}

#[tokio::test]
async fn changing_the_timezone_recomputes_the_slot_in_the_new_zone() {
    let fixture = fixture().await;
    let definition = create(&fixture, daily("09:00", "Asia/Shanghai")).await;
    let before = only_pending(&fixture, &definition.id).await;

    update_trigger(&fixture, &definition, daily("09:00", "Europe/Berlin"))
        .await
        .unwrap();

    let after = only_pending(&fixture, &definition.id).await;
    assert_ne!(
        after, before,
        "the same wall-clock text in another zone is another instant"
    );
    let local = after.with_timezone(&parse_timezone("Europe/Berlin").unwrap());
    assert_eq!((local.hour(), local.minute()), (9, 0));
}

/// A rule with no future occurrence cannot replace the slot. The update must
/// fail before the old slot is deleted; otherwise a successful-looking edit
/// would silently stop a schedule the stored rule claims is runnable.
#[tokio::test]
async fn a_replacement_with_no_future_occurrence_keeps_the_definition_and_slot() {
    let fixture = fixture().await;
    let definition = create(&fixture, daily("09:00", "UTC")).await;
    let before = only_pending(&fixture, &definition.id).await;

    // February 30 never occurs.
    let error = update_trigger(&fixture, &definition, cron("0 0 30 2 *", "UTC"))
        .await
        .expect_err("the update must not accept a rule that cannot run again");
    assert!(
        error.to_string().contains("has no next occurrence"),
        "unexpected error: {error}"
    );

    let reread = fixture
        .store
        .definition(&definition.id)
        .await
        .unwrap()
        .expect("the definition still exists");
    assert_eq!(reread.trigger, definition.trigger);
    assert_eq!(only_pending(&fixture, &definition.id).await, before);
}
