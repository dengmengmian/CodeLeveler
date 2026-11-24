//! Overlap policy and persisted unattended grants.
//!
//! These are store-level facts: the decision is made before a claim, the
//! outcome is visible in history, and an explicit run-now slot is still an
//! ordinary run subject to its definition's policy. Authorization is a stored
//! grant bound to the definition identity, never a flag captured elsewhere.

use std::path::PathBuf;
use std::time::Duration;

use leveler_client_protocol::{ModelRef, PermissionProfile};
use leveler_schedule::{
    ActionKind, MisfirePolicy, NewScheduleDefinition, OverlapPolicy, RunOutcome, RunState,
    ScheduleAction, ScheduleStore, ScheduleTrigger, SessionPolicy, UnattendedRequest,
};

async fn store() -> (ScheduleStore, tempfile::TempDir) {
    let tmp = tempfile::tempdir().unwrap();
    let store = ScheduleStore::open(&tmp.path().join("schedules.db"))
        .await
        .unwrap();
    (store, tmp)
}

async fn definition(
    store: &ScheduleStore,
    workspace: &std::path::Path,
    overlap: OverlapPolicy,
    session_policy: SessionPolicy,
    unattended: Option<UnattendedRequest>,
) -> leveler_schedule::ScheduleDefinition {
    store
        .create_definition(NewScheduleDefinition {
            name: "policy".into(),
            workspace: workspace.to_path_buf(),
            action: ScheduleAction::Agent {
                goal: "summarize".into(),
            },
            trigger: ScheduleTrigger::Every {
                anchor: leveler_core::now() - chrono::Duration::seconds(5),
                every: Duration::from_secs(3600),
            },
            session_policy,
            overlap,
            mode: PermissionProfile::Assisted,
            permission_contract: leveler_client_protocol::PermissionContract::V2,
            model: Some(ModelRef::new("deepseek", "deepseek-chat")),
            timeout: Duration::from_secs(30),
            misfire: MisfirePolicy::Skip,
            misfire_grace: Duration::from_secs(300),
            unattended,
        })
        .await
        .unwrap()
}

fn workspace(root: &std::path::Path) -> PathBuf {
    let workspace = root.join("workspace");
    std::fs::create_dir_all(&workspace).unwrap();
    workspace
}

#[tokio::test]
async fn skip_if_running_records_a_skipped_slot_and_runs_nothing() {
    let (store, tmp) = store().await;
    let definition = definition(
        &store,
        &workspace(tmp.path()),
        OverlapPolicy::SkipIfRunning,
        SessionPolicy::Continue,
        None,
    )
    .await;
    let now = leveler_core::now();

    let first = store
        .claim_due(now, Duration::from_secs(60), 10)
        .await
        .unwrap();
    assert_eq!(first.len(), 1);
    store.create_run_now(&definition).await.unwrap();

    assert_eq!(
        store.reconcile_overlaps(leveler_core::now()).await.unwrap(),
        1
    );
    assert!(
        store
            .claim_due(leveler_core::now(), Duration::from_secs(60), 10)
            .await
            .unwrap()
            .is_empty(),
        "a skipped slot is never claimable"
    );

    let runs = store.runs(Some(&definition.id), 10).await.unwrap();
    assert!(
        runs.iter().any(|run| run.state == RunState::Skipped
            && run
                .error
                .as_deref()
                .is_some_and(|error| error.contains("skip-if-running"))),
        "{runs:?}"
    );
}

#[tokio::test]
async fn queue_one_waits_for_the_active_run_then_claims_the_slot() {
    let (store, tmp) = store().await;
    let definition = definition(
        &store,
        &workspace(tmp.path()),
        OverlapPolicy::QueueOne,
        SessionPolicy::Continue,
        None,
    )
    .await;
    let now = leveler_core::now();

    let first = store
        .claim_due(now, Duration::from_secs(60), 10)
        .await
        .unwrap();
    assert_eq!(first.len(), 1);
    store.create_run_now(&definition).await.unwrap();
    assert_eq!(
        store.reconcile_overlaps(leveler_core::now()).await.unwrap(),
        0
    );
    assert!(
        store
            .claim_due(leveler_core::now(), Duration::from_secs(60), 10)
            .await
            .unwrap()
            .is_empty(),
        "queue-one leaves the slot pending while a run is active"
    );

    store
        .finish_run(&first[0].run_id, RunOutcome::Succeeded { exit_code: None })
        .await
        .unwrap();
    let claims = store
        .claim_due(leveler_core::now(), Duration::from_secs(60), 10)
        .await
        .unwrap();
    assert_eq!(claims.len(), 1, "the queued slot becomes claimable");
    assert!(claims[0].occurrence.run_now);
}

#[tokio::test]
async fn allow_concurrent_claims_a_second_slot_while_the_first_is_active() {
    let (store, tmp) = store().await;
    let definition = definition(
        &store,
        &workspace(tmp.path()),
        OverlapPolicy::AllowConcurrent,
        SessionPolicy::Fresh,
        None,
    )
    .await;
    let now = leveler_core::now();

    let first = store
        .claim_due(now, Duration::from_secs(60), 10)
        .await
        .unwrap();
    assert_eq!(first.len(), 1);
    store.create_run_now(&definition).await.unwrap();
    assert_eq!(
        store.reconcile_overlaps(leveler_core::now()).await.unwrap(),
        0
    );

    let second = store
        .claim_due(leveler_core::now(), Duration::from_secs(60), 10)
        .await
        .unwrap();
    assert_eq!(second.len(), 1);
    assert_ne!(second[0].run_id, first[0].run_id);
}

#[tokio::test]
async fn a_run_now_slot_is_claimable_while_paused_but_the_rule_is_not() {
    let (store, tmp) = store().await;
    let definition = definition(
        &store,
        &workspace(tmp.path()),
        OverlapPolicy::SkipIfRunning,
        SessionPolicy::Continue,
        None,
    )
    .await;
    store.set_enabled(&definition.id, false).await.unwrap();
    store.create_run_now(&definition).await.unwrap();

    let claims = store
        .claim_due(leveler_core::now(), Duration::from_secs(60), 10)
        .await
        .unwrap();
    assert_eq!(claims.len(), 1, "only the explicit slot is claimable");
    assert!(claims[0].occurrence.run_now);
}

#[tokio::test]
async fn an_unattended_grant_round_trips_and_revocation_takes_effect() {
    let (store, tmp) = store().await;
    let definition = definition(
        &store,
        &workspace(tmp.path()),
        OverlapPolicy::SkipIfRunning,
        SessionPolicy::Continue,
        Some(UnattendedRequest {
            allow_external_side_effects: true,
        }),
    )
    .await;

    let grant = definition.unattended.as_ref().expect("stored grant");
    assert_eq!(grant.definition_id, definition.id);
    assert_eq!(grant.workspace, definition.workspace);
    assert_eq!(grant.permission_mode, definition.mode);
    assert_eq!(grant.actions, vec![ActionKind::Agent]);
    assert!(grant.external_side_effects);
    definition
        .unattended_authorization(ActionKind::Agent, true)
        .expect("the grant covers this schedule");
    assert!(
        definition
            .unattended_authorization(ActionKind::Native, true)
            .is_err(),
        "a grant for one action kind does not cover another"
    );

    assert!(store.revoke_unattended(&definition.id).await.unwrap());
    assert!(!store.revoke_unattended(&definition.id).await.unwrap());
    let reloaded = store.definition(&definition.id).await.unwrap().unwrap();
    assert!(reloaded.unattended.is_none());
    assert!(
        reloaded
            .unattended_authorization(ActionKind::Agent, true)
            .is_err()
    );
}

#[tokio::test]
async fn deleting_an_active_definition_is_refused_until_the_run_settles() {
    let (store, tmp) = store().await;
    let definition = definition(
        &store,
        &workspace(tmp.path()),
        OverlapPolicy::SkipIfRunning,
        SessionPolicy::Continue,
        None,
    )
    .await;
    let claims = store
        .claim_due(leveler_core::now(), Duration::from_secs(60), 10)
        .await
        .unwrap();
    assert_eq!(claims.len(), 1);

    let error = store
        .remove_definition(&definition.id)
        .await
        .expect_err("an active run is never orphaned by deletion");
    assert!(error.to_string().contains("active"), "{error}");

    store
        .finish_run(
            &claims[0].run_id,
            RunOutcome::Succeeded { exit_code: Some(0) },
        )
        .await
        .unwrap();
    assert!(store.remove_definition(&definition.id).await.unwrap());
}

/// A stored unattended grant names the permission contract it was issued
/// under. Changing the schedule's contract (or reusing an older grant that
/// never named one) must fail closed, not be interpreted as the new meaning.
#[tokio::test]
async fn an_unattended_grant_never_covers_a_different_contract() {
    use leveler_client_protocol::PermissionContract;
    let (store, tmp) = store().await;
    let definition = definition(
        &store,
        &workspace(tmp.path()),
        OverlapPolicy::SkipIfRunning,
        SessionPolicy::Continue,
        None,
    )
    .await;
    let grant = leveler_schedule::UnattendedGrant::new(
        definition.id.clone(),
        definition.workspace.clone(),
        definition.mode,
        PermissionContract::V1,
        &definition.action,
        &definition.trigger,
        definition.model.clone(),
        UnattendedRequest {
            allow_external_side_effects: false,
        },
        leveler_core::now(),
    )
    .unwrap();
    let error = grant
        .covers(&definition, ActionKind::Agent, false)
        .unwrap_err();
    assert!(error.contains("permission contract"), "{error}");

    let mut legacy = grant.clone();
    legacy.version = 3;
    let error = legacy
        .covers(&definition, ActionKind::Agent, false)
        .unwrap_err();
    assert!(error.contains("grant version 3"), "{error}");
}
