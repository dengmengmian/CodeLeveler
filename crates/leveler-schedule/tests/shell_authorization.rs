//! A scheduled shell command is unattended execution.
//!
//! The runtime executes the command without a person present, so the
//! definition's persisted grant is the only authorization. These tests pin
//! that decision at the dispatcher seam: a missing grant must block before any
//! runtime connection, and a matching grant must let the action reach it.

#![cfg(unix)]

use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use async_trait::async_trait;
use leveler_client_protocol::PermissionProfile;
use leveler_local_transport::LocalRuntimeService;
use leveler_schedule::{
    ActionKind, Dispatcher, MisfirePolicy, NewScheduleDefinition, OverlapPolicy, RunState,
    RuntimeConnector, ScheduleAction, ScheduleStore, ScheduleTrigger, SessionPolicy,
    UnattendedRequest,
};

struct CountingConnector {
    attempts: Arc<AtomicUsize>,
}

#[async_trait]
impl RuntimeConnector for CountingConnector {
    async fn connect(&self, _workspace: &Path) -> Result<Arc<dyn LocalRuntimeService>, String> {
        self.attempts.fetch_add(1, Ordering::SeqCst);
        Err("the test runtime is intentionally unreachable".into())
    }
}

async fn fixture(
    mode: PermissionProfile,
    unattended: Option<UnattendedRequest>,
) -> (
    ScheduleStore,
    leveler_schedule::ScheduleId,
    tempfile::TempDir,
) {
    let tmp = tempfile::tempdir().unwrap();
    let workspace = tmp.path().join("workspace");
    std::fs::create_dir_all(&workspace).unwrap();
    let store = ScheduleStore::open(&tmp.path().join("schedules.db"))
        .await
        .unwrap();
    let definition = store
        .create_definition(NewScheduleDefinition {
            name: "shell authorization".into(),
            workspace,
            action: ScheduleAction::Shell {
                command: "printf scheduled".into(),
            },
            trigger: ScheduleTrigger::Once {
                at: leveler_core::now() - chrono::Duration::seconds(1),
            },
            session_policy: SessionPolicy::Continue,
            overlap: OverlapPolicy::SkipIfRunning,
            mode,
            model: None,
            timeout: Duration::from_secs(30),
            misfire: MisfirePolicy::Skip,
            misfire_grace: Duration::from_secs(300),
            unattended,
        })
        .await
        .unwrap();
    (store, definition.id, tmp)
}

#[tokio::test]
async fn a_shell_run_without_a_grant_is_blocked_before_delivery() {
    let (store, id, _tmp) = fixture(PermissionProfile::Assisted, None).await;
    let attempts = Arc::new(AtomicUsize::new(0));
    let dispatcher = Dispatcher::new(
        store.clone(),
        Arc::new(CountingConnector {
            attempts: attempts.clone(),
        }),
    )
    .with_lease(Duration::from_secs(30))
    .with_poll_interval(Duration::from_millis(10));

    let summary = dispatcher.dispatch_due(1).await.unwrap();
    assert_eq!(summary.blocked, 1, "{summary:?}");
    assert_eq!(
        attempts.load(Ordering::SeqCst),
        0,
        "authorization must be decided before the dispatcher connects to a runtime"
    );

    let run = store.runs(Some(&id), 1).await.unwrap().remove(0);
    assert_eq!(run.state, RunState::Blocked);
    assert!(
        run.error
            .as_deref()
            .is_some_and(|error| error.contains("unattended authorization")),
        "{run:?}"
    );
}

#[tokio::test]
async fn a_shell_run_with_a_matching_grant_reaches_the_runtime() {
    let (store, id, _tmp) = fixture(
        PermissionProfile::Assisted,
        Some(UnattendedRequest {
            allow_external_side_effects: true,
        }),
    )
    .await;
    let attempts = Arc::new(AtomicUsize::new(0));
    let dispatcher = Dispatcher::new(
        store.clone(),
        Arc::new(CountingConnector {
            attempts: attempts.clone(),
        }),
    )
    .with_lease(Duration::from_secs(30))
    .with_poll_interval(Duration::from_millis(10));

    let summary = dispatcher.dispatch_due(1).await.unwrap();
    assert_eq!(summary.failed, 1, "{summary:?}");
    assert_eq!(
        attempts.load(Ordering::SeqCst),
        1,
        "a matching grant must let the action reach the runtime"
    );

    let run = store.runs(Some(&id), 1).await.unwrap().remove(0);
    assert_eq!(run.state, RunState::Failed);
    assert!(
        run.error
            .as_deref()
            .is_some_and(|error| error.contains("could not connect")),
        "{run:?}"
    );
}

#[tokio::test]
async fn full_access_without_a_grant_does_not_authorize_a_shell_run() {
    let (store, id, _tmp) = fixture(PermissionProfile::FullAccess, None).await;
    let attempts = Arc::new(AtomicUsize::new(0));
    let dispatcher = Dispatcher::new(
        store.clone(),
        Arc::new(CountingConnector {
            attempts: attempts.clone(),
        }),
    )
    .with_lease(Duration::from_secs(30))
    .with_poll_interval(Duration::from_millis(10));

    let definition = store.definition(&id).await.unwrap().unwrap();
    definition
        .unattended_authorization(ActionKind::Shell, true)
        .expect_err("full access must not stand in for a durable grant");

    let summary = dispatcher.dispatch_due(1).await.unwrap();
    assert_eq!(summary.blocked, 1, "{summary:?}");
    assert_eq!(attempts.load(Ordering::SeqCst), 0);
    assert_eq!(
        store.runs(Some(&id), 1).await.unwrap()[0].state,
        RunState::Blocked
    );
}
