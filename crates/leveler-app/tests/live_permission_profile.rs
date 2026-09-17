//! The permission profile a session runs under is live state, not a value a
//! turn copies at startup.
//!
//! Before this, switching to 完全访问 while a turn was running updated the UI
//! and the session row immediately, while the running turn — and every agent
//! it had already delegated to — kept authorizing under the profile captured
//! when it started. These pin both directions of the change, and that one
//! session's change never reaches another.

use leveler_app::{Application, InProcessRuntimeClient};
use leveler_client_protocol::{
    ClientCommand, InteractiveRuntimeClient, PermissionProfile as WirePermissionProfile,
    RuntimeEvent,
};
use leveler_execution::PermissionProfile;
use leveler_project::Layout;

fn isolate_global_config() {
    use std::sync::OnceLock;
    static EMPTY_HOME: OnceLock<tempfile::TempDir> = OnceLock::new();
    let dir = EMPTY_HOME.get_or_init(|| tempfile::tempdir().unwrap());
    unsafe {
        std::env::set_var("LEVELER_HOME", dir.path());
    }
}

fn app(tmp: &tempfile::TempDir) -> Application {
    Application::assemble(Layout::from_parts(
        tmp.path().to_path_buf(),
        tmp.path().join("configs"),
        tmp.path().join("state"),
    ))
    .unwrap()
}

/// A session that has never run a turn holds no live cell yet; the next turn
/// starts from the persisted value, so reporting "changed" would be a lie.
#[test]
fn a_session_with_no_running_turn_reports_no_live_profile() {
    isolate_global_config();
    let tmp = tempfile::tempdir().unwrap();
    let app = app(&tmp);
    assert_eq!(app.live_permission_profile("never-ran"), None);
    assert!(
        !app.set_live_permission_profile("never-ran", PermissionProfile::FullAccess),
        "there is no running execution to apply it to"
    );
}

/// The daemon's update path: one change is seen by the session's running
/// execution, in both directions, with no turn restart.
#[tokio::test]
async fn a_permission_change_reaches_a_sessions_running_execution() {
    isolate_global_config();
    let tmp = tempfile::tempdir().unwrap();
    let app = app(&tmp);

    // Building an engine is what gives a session its live cell.
    let engine = app
        .engine_for_with_profile(
            &leveler_model::ModelRef::new("mock", "m"),
            PermissionProfile::Assisted,
            false,
            std::sync::Arc::new(leveler_execution::AutoApprove),
            std::sync::Arc::new(leveler_agent::AutoClarify),
            leveler_agent::WorkProfile::Balanced,
            false,
            Some("session-a"),
        )
        .await
        .unwrap();
    assert_eq!(
        engine.factory.tool_context.policy.mode(),
        PermissionProfile::Assisted
    );

    assert!(app.set_live_permission_profile("session-a", PermissionProfile::FullAccess));
    assert_eq!(
        engine.factory.tool_context.policy.mode(),
        PermissionProfile::FullAccess,
        "the running execution must observe the upgrade without being rebuilt"
    );

    assert!(app.set_live_permission_profile("session-a", PermissionProfile::RequestApproval));
    assert_eq!(
        engine.factory.tool_context.policy.mode(),
        PermissionProfile::RequestApproval,
        "and the downgrade, which is the security-relevant direction"
    );
}

/// One daemon hosts several sessions at different profiles. Changing one must
/// not touch another.
#[tokio::test]
async fn one_sessions_permission_change_does_not_move_another() {
    isolate_global_config();
    let tmp = tempfile::tempdir().unwrap();
    let app = app(&tmp);
    let model = leveler_model::ModelRef::new("mock", "m");

    let mut engines = Vec::new();
    for scope in ["session-a", "session-b"] {
        engines.push(
            app.engine_for_with_profile(
                &model,
                PermissionProfile::Assisted,
                false,
                std::sync::Arc::new(leveler_execution::AutoApprove),
                std::sync::Arc::new(leveler_agent::AutoClarify),
                leveler_agent::WorkProfile::Balanced,
                false,
                Some(scope),
            )
            .await
            .unwrap(),
        );
    }

    app.set_live_permission_profile("session-a", PermissionProfile::FullAccess);

    assert_eq!(
        engines[0].factory.tool_context.policy.mode(),
        PermissionProfile::FullAccess
    );
    assert_eq!(
        engines[1].factory.tool_context.policy.mode(),
        PermissionProfile::Assisted,
        "another session must keep the profile its user chose"
    );
    assert_eq!(
        app.live_permission_profile("session-b"),
        Some(PermissionProfile::Assisted)
    );
}

/// A later turn of the same session reuses the session's cell, so it starts
/// from the profile in force — not from a stale one.
#[tokio::test]
async fn a_later_turn_of_the_same_session_reuses_the_live_profile() {
    isolate_global_config();
    let tmp = tempfile::tempdir().unwrap();
    let app = app(&tmp);
    let model = leveler_model::ModelRef::new("mock", "m");
    let build = |mode| {
        app.engine_for_with_profile(
            &model,
            mode,
            false,
            std::sync::Arc::new(leveler_execution::AutoApprove),
            std::sync::Arc::new(leveler_agent::AutoClarify),
            leveler_agent::WorkProfile::Balanced,
            false,
            Some("session-a"),
        )
    };

    let first = build(PermissionProfile::Assisted).await.unwrap();
    app.set_live_permission_profile("session-a", PermissionProfile::FullAccess);

    // The next turn is built from the caller's current authority, which the
    // daemon reads from the same session config it just persisted.
    let second = build(PermissionProfile::FullAccess).await.unwrap();
    assert_eq!(
        second.factory.tool_context.policy.mode(),
        PermissionProfile::FullAccess
    );
    assert_eq!(
        first.factory.tool_context.policy.mode(),
        PermissionProfile::FullAccess,
        "one cell per session — turns do not fork it"
    );
}

/// The product path: `SetPermissionProfile` persists the session row AND
/// updates the live cell a running engine is already bound to, before the
/// TUI is allowed to show the new value (`SessionUpdated` follows persist).
#[tokio::test]
async fn set_permission_profile_reaches_a_running_engine_before_session_updated() {
    isolate_global_config();
    let tmp = tempfile::tempdir().unwrap();
    let app = std::sync::Arc::new(app(&tmp));
    let model = leveler_model::ModelRef::new("mock", "m");
    let session_id = app.create_session(&model, "goal").await.unwrap();

    let engine = app
        .engine_for_with_profile(
            &model,
            PermissionProfile::Assisted,
            false,
            std::sync::Arc::new(leveler_execution::AutoApprove),
            std::sync::Arc::new(leveler_agent::AutoClarify),
            leveler_agent::WorkProfile::Balanced,
            false,
            Some(session_id.as_str()),
        )
        .await
        .unwrap();
    assert_eq!(
        engine.factory.tool_context.policy.mode(),
        PermissionProfile::Assisted
    );

    let client =
        InProcessRuntimeClient::new(app.clone(), model, PermissionProfile::Assisted, false);
    let mut events = client.subscribe_session(&session_id);
    client
        .send(ClientCommand::SetPermissionProfile {
            session_id: session_id.clone(),
            mode: WirePermissionProfile::FullAccess,
        })
        .await
        .unwrap();

    assert_eq!(
        engine.factory.tool_context.policy.mode(),
        PermissionProfile::FullAccess,
        "the running engine must observe the command before any UI ack"
    );
    assert_eq!(
        app.live_permission_profile(session_id.as_str()),
        Some(PermissionProfile::FullAccess)
    );
    let event = events.recv().await.expect("SessionUpdated follows persist");
    assert!(
        matches!(
            event,
            RuntimeEvent::SessionUpdated { ref session }
                if session.mode == WirePermissionProfile::FullAccess
        ),
        "UI ack must trail the live cell, got {event:?}"
    );
}
