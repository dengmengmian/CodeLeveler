//! The plan a reconnecting client sees after the runtime restarted.
//!
//! `update_plan` is persisted as `plan_updated` and a resumed turn seeds its
//! plan from that row. The reconnect snapshot must answer from the same row:
//! if it only knows what the current process happened to forward, a restarted
//! daemon reports "no plan" to the user while the agent still works from 2/5.

use std::sync::Arc;

use leveler_app::{Application, InProcessRuntimeClient};
use leveler_client_protocol::{InteractiveRuntimeClient, PlanStepStatus};
use leveler_core::SessionId;
use leveler_engine::EngineEvent;
use leveler_execution::PermissionProfile;
use leveler_lifecycle::{StopReason, TaskOutcome, VerificationStatus};
use leveler_model::ModelRef;
use leveler_project::Layout;
use leveler_storage::EventRepository;

/// Point `LEVELER_HOME` at an empty dir so `GlobalConfig::load()` yields the
/// default. Tests must not depend on the developer's `~/.leveler/config.toml`.
fn isolate_global_config() {
    use std::sync::OnceLock;
    static EMPTY_HOME: OnceLock<tempfile::TempDir> = OnceLock::new();
    let dir = EMPTY_HOME.get_or_init(|| tempfile::tempdir().unwrap());
    unsafe {
        std::env::set_var("LEVELER_HOME", dir.path());
    }
}

fn layout(root: &std::path::Path) -> Layout {
    Layout::from_parts(root.to_path_buf(), root.join("configs"), root.join("state"))
}

fn client(app: Arc<Application>) -> InProcessRuntimeClient {
    InProcessRuntimeClient::new(
        app,
        ModelRef::new("mock", "m"),
        PermissionProfile::RequestApproval,
        false,
    )
}

async fn persist_plan(app: &Application, session_id: &SessionId, steps: serde_json::Value) {
    let db = app.open_database().await.unwrap();
    let payload = serde_json::json!({"type": "plan_updated", "payload": {"steps": steps}});
    EventRepository::new(&db)
        .append(
            session_id,
            None,
            "plan_updated",
            &payload.to_string(),
            leveler_core::now(),
        )
        .await
        .unwrap();
}

async fn persist_task_terminal(app: &Application, session_id: &SessionId) {
    let db = app.open_database().await.unwrap();
    let event = EngineEvent::TaskFinished {
        outcome: TaskOutcome::Completed,
        verification: VerificationStatus::NotRun,
        reason: None,
        stop: Some(StopReason::Completed),
        failure: None,
        warnings: Vec::new(),
    };
    let (event_type, payload) = event.to_row().unwrap();
    EventRepository::new(&db)
        .append(session_id, None, &event_type, &payload, leveler_core::now())
        .await
        .unwrap();
}

#[tokio::test]
async fn a_restarted_runtime_reports_the_persisted_plan() {
    isolate_global_config();
    let tmp = tempfile::tempdir().unwrap();

    let session_id = {
        let app = Arc::new(Application::assemble(layout(tmp.path())).unwrap());
        let session_id = app
            .create_session(&ModelRef::new("mock", "m"), "plan restart")
            .await
            .unwrap();
        persist_plan(
            &app,
            &session_id,
            serde_json::json!([
                {"step": "a", "status": "completed"},
                {"step": "b", "status": "completed"},
                {"step": "c", "status": "in_progress"},
                {"step": "d", "status": "pending"},
                {"step": "e", "status": "pending"},
            ]),
        )
        .await;
        session_id
        // The process ends here: only the state directory carries over.
    };

    let app = Arc::new(Application::assemble(layout(tmp.path())).unwrap());
    let snapshot = client(app).snapshot(&session_id).await.unwrap();

    let plan = snapshot
        .plan
        .expect("the persisted plan must survive the restart");
    let statuses: Vec<_> = plan.steps.iter().map(|s| s.status).collect();
    assert_eq!(
        statuses,
        [
            PlanStepStatus::Done,
            PlanStepStatus::Done,
            PlanStepStatus::Running,
            PlanStepStatus::Pending,
            PlanStepStatus::Pending,
        ]
    );
    assert_eq!(plan.steps[2].description, "c");
}

/// A context cut persists an empty plan; the restarted snapshot must not
/// resurrect the plan from before the cut.
#[tokio::test]
async fn a_restarted_runtime_honours_a_cleared_plan() {
    isolate_global_config();
    let tmp = tempfile::tempdir().unwrap();

    let session_id = {
        let app = Arc::new(Application::assemble(layout(tmp.path())).unwrap());
        let session_id = app
            .create_session(&ModelRef::new("mock", "m"), "plan cleared")
            .await
            .unwrap();
        persist_plan(
            &app,
            &session_id,
            serde_json::json!([{"step": "old", "status": "in_progress"}]),
        )
        .await;
        persist_plan(&app, &session_id, serde_json::json!([])).await;
        session_id
    };

    let app = Arc::new(Application::assemble(layout(tmp.path())).unwrap());
    let snapshot = client(app).snapshot(&session_id).await.unwrap();
    assert!(
        snapshot.plan.is_none_or(|plan| plan.steps.is_empty()),
        "the cut plan must stay cut"
    );
}

#[tokio::test]
async fn a_restarted_runtime_does_not_resurrect_a_plan_from_a_terminal_task_epoch() {
    isolate_global_config();
    let tmp = tempfile::tempdir().unwrap();

    let session_id = {
        let app = Arc::new(Application::assemble(layout(tmp.path())).unwrap());
        let session_id = app
            .create_session(&ModelRef::new("mock", "m"), "terminal plan")
            .await
            .unwrap();
        persist_plan(
            &app,
            &session_id,
            serde_json::json!([
                {"step": "done", "status": "completed"},
                {"step": "not done", "status": "in_progress"}
            ]),
        )
        .await;
        persist_task_terminal(&app, &session_id).await;
        session_id
    };

    let app = Arc::new(Application::assemble(layout(tmp.path())).unwrap());
    let snapshot = client(app).snapshot(&session_id).await.unwrap();

    assert!(
        snapshot.plan.is_none(),
        "a terminal epoch's final plan is history, not an active reconnect plan"
    );
}

#[tokio::test]
async fn a_plan_updated_after_the_latest_terminal_is_active_on_restart() {
    isolate_global_config();
    let tmp = tempfile::tempdir().unwrap();

    let session_id = {
        let app = Arc::new(Application::assemble(layout(tmp.path())).unwrap());
        let session_id = app
            .create_session(&ModelRef::new("mock", "m"), "next epoch plan")
            .await
            .unwrap();
        persist_plan(
            &app,
            &session_id,
            serde_json::json!([{"step": "historical", "status": "in_progress"}]),
        )
        .await;
        persist_task_terminal(&app, &session_id).await;
        persist_plan(
            &app,
            &session_id,
            serde_json::json!([{"step": "current", "status": "in_progress"}]),
        )
        .await;
        session_id
    };

    let app = Arc::new(Application::assemble(layout(tmp.path())).unwrap());
    let snapshot = client(app).snapshot(&session_id).await.unwrap();
    let plan = snapshot
        .plan
        .expect("the new epoch plan must remain active");

    assert_eq!(plan.steps.len(), 1);
    assert_eq!(plan.steps[0].description, "current");
    assert_eq!(plan.steps[0].status, PlanStepStatus::Running);
}
