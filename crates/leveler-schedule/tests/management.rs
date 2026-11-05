//! Structured management contract: authoritative projections, optimistic
//! concurrency, grant invalidation and the filters every surface shares.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use leveler_client_protocol::schedule::{
    ScheduleActionWire, ScheduleDraft, ScheduleFilter, SchedulePatch, ScheduleRequest,
    ScheduleResponse, ScheduleTriggerWire, ScheduleView,
};
use leveler_schedule::{RunOutcome, ScheduleManager, ScheduleStore};

struct Fixture {
    manager: ScheduleManager,
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
        manager: ScheduleManager::new(Arc::new(store), tmp.path().to_path_buf(), None),
        workspace,
        _tmp: tmp,
    }
}

fn draft(name: &str, workspace: &std::path::Path, trigger: ScheduleTriggerWire) -> ScheduleDraft {
    ScheduleDraft {
        name: name.into(),
        workspace: workspace.display().to_string(),
        action: ScheduleActionWire::Shell {
            command: "echo scheduled".into(),
        },
        trigger,
        session_policy: None,
        overlap: None,
        mode: Some("full_access".into()),
        model: None,
        timeout_secs: None,
        misfire: None,
        misfire_grace_secs: None,
        grant_unattended: false,
        allow_external_side_effects: false,
    }
}

async fn create(manager: &ScheduleManager, draft: ScheduleDraft) -> ScheduleView {
    match manager.handle(ScheduleRequest::Create { draft }).await {
        ScheduleResponse::Changed {
            schedule: Some(schedule),
            ..
        } => schedule,
        other => panic!("unexpected create response: {other:?}"),
    }
}

async fn show(manager: &ScheduleManager, id: &str) -> ScheduleView {
    match manager
        .handle(ScheduleRequest::Show { id: id.into() })
        .await
    {
        ScheduleResponse::Show { schedule } => schedule,
        other => panic!("unexpected show response: {other:?}"),
    }
}

#[tokio::test]
async fn create_and_update_report_a_change_not_a_read() {
    let fixture = fixture().await;

    let response = fixture
        .manager
        .handle(ScheduleRequest::Create {
            draft: draft("fresh", &fixture.workspace, daily("09:00")),
        })
        .await;
    let (created, message) = match response {
        ScheduleResponse::Changed {
            schedule: Some(schedule),
            message,
        } => (schedule, message),
        other => panic!("create must report a change, got {other:?}"),
    };
    assert!(!message.is_empty());
    assert_eq!(created.name, "fresh");

    let response = fixture
        .manager
        .handle(ScheduleRequest::Update {
            id: created.id.clone(),
            expected_revision: Some(created.revision.clone()),
            patch: SchedulePatch {
                name: Some("renamed".into()),
                ..SchedulePatch::default()
            },
        })
        .await;
    let updated = match response {
        ScheduleResponse::Changed {
            schedule: Some(schedule),
            ..
        } => schedule,
        other => panic!("update must report a change, got {other:?}"),
    };
    assert_eq!(updated.name, "renamed");
}

fn error(response: ScheduleResponse) -> String {
    match response {
        ScheduleResponse::Error { message } => message,
        other => panic!("expected an error, got {other:?}"),
    }
}

fn daily(time: &str) -> ScheduleTriggerWire {
    ScheduleTriggerWire::Daily {
        time: time.into(),
        timezone: Some("Asia/Shanghai".into()),
    }
}

#[tokio::test]
async fn create_returns_the_stored_definition_with_a_real_next_slot() {
    let fixture = fixture().await;
    let view = create(
        &fixture.manager,
        draft("morning report", &fixture.workspace, daily("09:00")),
    )
    .await;

    assert_eq!(view.name, "morning report");
    assert_eq!(view.action_kind, "shell");
    assert_eq!(view.timezone.as_deref(), Some("Asia/Shanghai"));
    assert_eq!(view.trigger_text, "daily at 09:00 (Asia/Shanghai)");
    assert!(view.enabled);
    assert!(!view.running);
    assert!(view.next_run_at.is_some());
    assert!(!view.grant.granted);
    assert!(
        view.grant.requires_regrant,
        "full access without a persisted grant is not unattended authorization"
    );
    assert!(view.needs_attention);
    assert!(!view.revision.is_empty());
    let next = chrono::DateTime::parse_from_rfc3339(view.next_run_at.as_deref().unwrap())
        .unwrap()
        .with_timezone(&chrono::Utc);
    assert!(next > leveler_core::now());
}

#[tokio::test]
async fn a_calendar_rule_without_a_zone_uses_the_host_zone() {
    let fixture = fixture().await;
    let view = create(
        &fixture.manager,
        draft(
            "host zone",
            &fixture.workspace,
            ScheduleTriggerWire::Daily {
                time: "07:30".into(),
                timezone: None,
            },
        ),
    )
    .await;

    let expected = leveler_schedule::host_timezone().unwrap().to_string();
    assert_eq!(view.timezone.as_deref(), Some(expected.as_str()));
}

#[tokio::test]
async fn update_refuses_a_revision_the_client_no_longer_owns() {
    let fixture = fixture().await;
    let original = create(
        &fixture.manager,
        draft("first", &fixture.workspace, daily("09:00")),
    )
    .await;

    let response = fixture
        .manager
        .handle(ScheduleRequest::Update {
            id: original.id.clone(),
            expected_revision: Some(original.revision.clone()),
            patch: SchedulePatch {
                name: Some("second".into()),
                ..SchedulePatch::default()
            },
        })
        .await;
    assert!(matches!(response, ScheduleResponse::Changed { .. }));

    let error = error(
        fixture
            .manager
            .handle(ScheduleRequest::Update {
                id: original.id.clone(),
                expected_revision: Some(original.revision.clone()),
                patch: SchedulePatch {
                    name: Some("third".into()),
                    ..SchedulePatch::default()
                },
            })
            .await,
    );
    assert!(
        error.contains("changed since it was read"),
        "unexpected conflict message: {error}"
    );
    let current = show(&fixture.manager, &original.id).await;
    assert_eq!(current.name, "second");
}

#[tokio::test]
async fn a_name_only_update_does_not_shift_the_pending_slot() {
    let fixture = fixture().await;
    let at = leveler_core::now() - chrono::Duration::seconds(30);
    let view = create(
        &fixture.manager,
        draft(
            "once",
            &fixture.workspace,
            ScheduleTriggerWire::Once {
                at: at.to_rfc3339(),
            },
        ),
    )
    .await;
    let before = view.next_run_at.clone();

    assert!(matches!(
        fixture
            .manager
            .handle(ScheduleRequest::Update {
                id: view.id.clone(),
                expected_revision: Some(view.revision.clone()),
                patch: SchedulePatch {
                    name: Some("renamed".into()),
                    ..SchedulePatch::default()
                },
            })
            .await,
        ScheduleResponse::Changed { .. }
    ));

    let after = show(&fixture.manager, &view.id).await;
    assert_eq!(after.name, "renamed");
    assert_eq!(
        after.next_run_at, before,
        "renaming must not drop or move a pending one-off slot"
    );
}

#[tokio::test]
async fn changing_the_action_invalidates_the_unattended_grant() {
    let fixture = fixture().await;
    let mut created = draft("granted", &fixture.workspace, daily("09:00"));
    created.mode = Some("assisted".into());
    created.grant_unattended = true;
    created.allow_external_side_effects = true;
    let view = create(&fixture.manager, created).await;
    assert!(view.grant.granted);
    assert!(!view.grant.requires_regrant);

    let updated = match fixture
        .manager
        .handle(ScheduleRequest::Update {
            id: view.id.clone(),
            expected_revision: Some(view.revision.clone()),
            patch: SchedulePatch {
                action: Some(ScheduleActionWire::Shell {
                    command: "echo changed".into(),
                }),
                ..SchedulePatch::default()
            },
        })
        .await
    {
        ScheduleResponse::Changed {
            schedule: Some(schedule),
            ..
        } => schedule,
        other => panic!("unexpected update response: {other:?}"),
    };
    assert!(
        !updated.grant.granted,
        "a changed action drops the old grant"
    );
    assert!(updated.grant.requires_regrant);
    assert!(updated.needs_attention);
}

#[tokio::test]
async fn a_shell_grant_must_name_its_external_side_effects() {
    let fixture = fixture().await;
    let mut requested = draft("external", &fixture.workspace, daily("09:00"));
    requested.mode = Some("assisted".into());
    requested.grant_unattended = true;
    requested.allow_external_side_effects = false;
    match fixture
        .manager
        .handle(ScheduleRequest::Create { draft: requested })
        .await
    {
        ScheduleResponse::Error { message } => {
            assert!(message.contains("external side effects"), "{message}");
        }
        other => panic!("unexpected create response: {other:?}"),
    }
    assert!(
        fixture
            .manager
            .store()
            .definitions()
            .await
            .unwrap()
            .is_empty(),
        "a refused grant must not store the definition"
    );

    let view = create(
        &fixture.manager,
        draft("external", &fixture.workspace, daily("09:00")),
    )
    .await;
    match fixture
        .manager
        .handle(ScheduleRequest::Grant {
            id: view.id.clone(),
            allow_external_side_effects: false,
        })
        .await
    {
        ScheduleResponse::Error { message } => {
            assert!(message.contains("external side effects"), "{message}");
        }
        other => panic!("unexpected grant response: {other:?}"),
    }
    let shown = show(&fixture.manager, &view.id).await;
    assert!(
        !shown.grant.granted,
        "a grant that does not name its scope must not be stored"
    );
}

/// A create draft for a shell action with an explicitly issued grant. A
/// shell command can write outside the workspace, so the grant has to name
/// that scope; the trigger is the only field a later update changes.
fn granted(name: &str, workspace: &std::path::Path, trigger: ScheduleTriggerWire) -> ScheduleDraft {
    let mut draft = draft(name, workspace, trigger);
    draft.mode = Some("assisted".into());
    draft.grant_unattended = true;
    draft.allow_external_side_effects = true;
    draft
}

async fn update_trigger(
    manager: &ScheduleManager,
    view: &ScheduleView,
    trigger: ScheduleTriggerWire,
) -> ScheduleView {
    match manager
        .handle(ScheduleRequest::Update {
            id: view.id.clone(),
            expected_revision: Some(view.revision.clone()),
            patch: SchedulePatch {
                trigger: Some(trigger),
                ..SchedulePatch::default()
            },
        })
        .await
    {
        ScheduleResponse::Changed {
            schedule: Some(schedule),
            ..
        } => schedule,
        ScheduleResponse::Error { message } => panic!("update was refused: {message}"),
        other => panic!("unexpected update response: {other:?}"),
    }
}

#[tokio::test]
async fn increasing_the_trigger_frequency_invalidates_the_unattended_grant() {
    let fixture = fixture().await;
    let view = create(
        &fixture.manager,
        granted("cadence", &fixture.workspace, daily("09:00")),
    )
    .await;
    assert!(view.grant.granted);
    assert!(!view.grant.requires_regrant);

    // Seven runs per week became one run per minute. The old grant named a
    // cadence the user accepted; it cannot silently cover the new one.
    let updated = update_trigger(
        &fixture.manager,
        &view,
        ScheduleTriggerWire::Every {
            every_secs: 60,
            anchor: None,
        },
    )
    .await;
    assert!(
        !updated.grant.granted,
        "a frequency increase must drop the old grant: {updated:?}"
    );
    assert!(updated.grant.requires_regrant);
    assert!(updated.needs_attention);
}

#[tokio::test]
async fn changing_the_timezone_invalidates_the_unattended_grant() {
    let fixture = fixture().await;
    let view = create(
        &fixture.manager,
        granted("zone", &fixture.workspace, daily("09:00")),
    )
    .await;

    // The same wall-clock text in a different zone runs in a different
    // execution window. The cadence count is unchanged, the scope is not.
    let updated = update_trigger(
        &fixture.manager,
        &view,
        ScheduleTriggerWire::Daily {
            time: "09:00".into(),
            timezone: Some("UTC".into()),
        },
    )
    .await;
    assert!(
        !updated.grant.granted,
        "a timezone change must drop the old grant: {updated:?}"
    );
    assert!(updated.grant.requires_regrant);
}

#[tokio::test]
async fn a_time_only_change_keeps_the_unattended_grant() {
    let fixture = fixture().await;
    let view = create(
        &fixture.manager,
        granted("adjust", &fixture.workspace, daily("09:00")),
    )
    .await;

    let updated = update_trigger(
        &fixture.manager,
        &view,
        ScheduleTriggerWire::Daily {
            time: "10:15".into(),
            timezone: Some("Asia/Shanghai".into()),
        },
    )
    .await;
    assert!(
        updated.grant.granted,
        "a same-zone time adjustment is not a scope expansion: {updated:?}"
    );
    assert!(!updated.grant.requires_regrant);
    assert_eq!(updated.trigger_text, "daily at 10:15 (Asia/Shanghai)");
}

#[tokio::test]
async fn turning_a_one_off_into_a_recurring_rule_invalidates_the_grant() {
    let fixture = fixture().await;
    let view = create(
        &fixture.manager,
        granted(
            "one-off",
            &fixture.workspace,
            ScheduleTriggerWire::Once {
                at: (leveler_core::now() + chrono::Duration::hours(1)).to_rfc3339(),
            },
        ),
    )
    .await;

    let updated = update_trigger(&fixture.manager, &view, daily("09:00")).await;
    assert!(
        !updated.grant.granted,
        "one execution became an unbounded recurring rule: {updated:?}"
    );
    assert!(updated.grant.requires_regrant);
}

#[tokio::test]
async fn decreasing_the_frequency_keeps_the_unattended_grant() {
    let fixture = fixture().await;
    let view = create(
        &fixture.manager,
        granted(
            "slower",
            &fixture.workspace,
            ScheduleTriggerWire::Every {
                every_secs: 3600,
                anchor: None,
            },
        ),
    )
    .await;

    let updated = update_trigger(
        &fixture.manager,
        &view,
        ScheduleTriggerWire::Every {
            every_secs: 7200,
            anchor: None,
        },
    )
    .await;
    assert!(
        updated.grant.granted,
        "running less often does not exceed the grant: {updated:?}"
    );
    assert!(!updated.grant.requires_regrant);
}

#[tokio::test]
async fn a_schedule_without_a_grant_is_flagged_for_authorization() {
    let fixture = fixture().await;
    let mut created = draft("no grant", &fixture.workspace, daily("09:00"));
    created.mode = Some("assisted".into());
    let view = create(&fixture.manager, created).await;

    assert!(!view.grant.granted);
    assert!(view.grant.requires_regrant);
    assert!(view.needs_attention);
}

#[tokio::test]
async fn pausing_hides_the_generated_slot_but_keeps_run_now_claimable() {
    let fixture = fixture().await;
    let view = create(
        &fixture.manager,
        draft("pausable", &fixture.workspace, daily("09:00")),
    )
    .await;
    assert!(view.next_run_at.is_some());

    let paused = match fixture
        .manager
        .handle(ScheduleRequest::Pause {
            id: view.id.clone(),
        })
        .await
    {
        ScheduleResponse::Changed { schedule, .. } => schedule.unwrap(),
        other => panic!("unexpected pause response: {other:?}"),
    };
    assert!(!paused.enabled);
    assert!(
        paused.next_run_at.is_none(),
        "a paused schedule's generated slot is not going to run"
    );

    let queued = match fixture
        .manager
        .handle(ScheduleRequest::RunNow {
            id: view.id.clone(),
        })
        .await
    {
        ScheduleResponse::Changed { schedule, .. } => schedule.unwrap(),
        other => panic!("unexpected run-now response: {other:?}"),
    };
    assert!(queued.next_run_at.is_some());

    let claims = fixture
        .manager
        .store()
        .claim_due(leveler_core::now(), Duration::from_secs(60), 10)
        .await
        .unwrap();
    assert_eq!(claims.len(), 1);
    assert_eq!(claims[0].definition.id.as_str(), view.id);
    assert!(claims[0].occurrence.run_now);
}

#[tokio::test]
async fn list_filters_read_the_same_projection_as_show() {
    let fixture = fixture().await;
    let enabled = create(
        &fixture.manager,
        draft("enabled", &fixture.workspace, daily("09:00")),
    )
    .await;
    let paused = create(
        &fixture.manager,
        draft("paused", &fixture.workspace, daily("10:00")),
    )
    .await;
    fixture
        .manager
        .handle(ScheduleRequest::Pause {
            id: paused.id.clone(),
        })
        .await;

    let enabled_list = match fixture
        .manager
        .handle(ScheduleRequest::List {
            filter: Some(ScheduleFilter::Enabled),
        })
        .await
    {
        ScheduleResponse::List { schedules } => schedules,
        other => panic!("unexpected list response: {other:?}"),
    };
    assert_eq!(enabled_list.len(), 1);
    assert_eq!(enabled_list[0].id, enabled.id);

    let paused_list = match fixture
        .manager
        .handle(ScheduleRequest::List {
            filter: Some(ScheduleFilter::Paused),
        })
        .await
    {
        ScheduleResponse::List { schedules } => schedules,
        other => panic!("unexpected list response: {other:?}"),
    };
    assert_eq!(paused_list.len(), 1);
    assert_eq!(paused_list[0].id, paused.id);
}

#[tokio::test]
async fn recent_failure_filter_follows_the_latest_run_state() {
    let fixture = fixture().await;
    let view = create(
        &fixture.manager,
        draft(
            "failing",
            &fixture.workspace,
            ScheduleTriggerWire::Once {
                at: (leveler_core::now() - chrono::Duration::seconds(30)).to_rfc3339(),
            },
        ),
    )
    .await;
    let store = fixture.manager.store().clone();
    let claim = store
        .claim_due(leveler_core::now(), Duration::from_secs(60), 1)
        .await
        .unwrap()
        .remove(0);
    store
        .finish_run(
            &claim.run_id,
            RunOutcome::Failed {
                exit_code: Some(1),
                error: "exit status 1".into(),
            },
        )
        .await
        .unwrap();

    let failing = match fixture
        .manager
        .handle(ScheduleRequest::List {
            filter: Some(ScheduleFilter::RecentFailure),
        })
        .await
    {
        ScheduleResponse::List { schedules } => schedules,
        other => panic!("unexpected list response: {other:?}"),
    };
    assert_eq!(failing.len(), 1);
    assert_eq!(failing[0].id, view.id);
    assert_eq!(failing[0].run_state.as_deref(), Some("failed"));
    assert_eq!(failing[0].last_run.as_ref().unwrap().exit_code, Some(1));
    assert!(failing[0].needs_attention);
}

#[tokio::test]
async fn status_counts_definitions_and_the_next_slot_from_storage() {
    let fixture = fixture().await;
    create(
        &fixture.manager,
        draft("one", &fixture.workspace, daily("09:00")),
    )
    .await;
    let second = create(
        &fixture.manager,
        draft("two", &fixture.workspace, daily("10:00")),
    )
    .await;
    fixture
        .manager
        .handle(ScheduleRequest::Pause {
            id: second.id.clone(),
        })
        .await;

    let status = match fixture.manager.handle(ScheduleRequest::Status).await {
        ScheduleResponse::Status { status } => status,
        other => panic!("unexpected status response: {other:?}"),
    };
    assert_eq!(status.definitions, 2);
    assert_eq!(status.enabled, 1);
    assert_eq!(status.paused, 1);
    assert_eq!(status.running, 0);
    assert!(status.next_occurrence.is_some());
}
