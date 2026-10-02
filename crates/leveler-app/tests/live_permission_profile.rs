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
        .engine_for_session(
            &leveler_model::ModelRef::new("mock", "m"),
            PermissionProfile::Assisted,
            false,
            std::sync::Arc::new(leveler_execution::AutoApprove),
            std::sync::Arc::new(leveler_agent::AutoClarify),
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
            app.engine_for_session(
                &model,
                PermissionProfile::Assisted,
                false,
                std::sync::Arc::new(leveler_execution::AutoApprove),
                std::sync::Arc::new(leveler_agent::AutoClarify),
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
        app.engine_for_session(
            &model,
            mode,
            false,
            std::sync::Arc::new(leveler_execution::AutoApprove),
            std::sync::Arc::new(leveler_agent::AutoClarify),
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
        .engine_for_session(
            &model,
            PermissionProfile::Assisted,
            false,
            std::sync::Arc::new(leveler_execution::AutoApprove),
            std::sync::Arc::new(leveler_agent::AutoClarify),
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

#[tokio::test]
async fn delivered_permission_selection_persists_only_the_target_session_and_leaves_global_config_unchanged()
 {
    use leveler_client_protocol::{CommandEnvelope, CommandId};
    isolate_global_config();
    let tmp = tempfile::tempdir().unwrap();
    let app = std::sync::Arc::new(app(&tmp));
    let model = leveler_model::ModelRef::new("mock", "m");
    let session_id = app.create_session(&model, "target").await.unwrap();
    let other = app.create_session(&model, "other").await.unwrap();
    let client = InProcessRuntimeClient::new(
        app.clone(),
        model.clone(),
        PermissionProfile::Assisted,
        false,
    );
    let config_path = leveler_app::global_config::GlobalConfig::path().unwrap();
    fn config_bytes(path: &std::path::Path) -> Option<Vec<u8>> {
        match std::fs::read(path) {
            Ok(bytes) => Some(bytes),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
            Err(error) => panic!("isolated config read failed: {error}"),
        }
    }
    let global_before = config_bytes(&config_path);
    let initial = client.snapshot(&session_id).await.unwrap().mode;
    let other_before = client.snapshot(&other).await.unwrap().mode;
    let mut events = client.subscribe_session(&session_id);
    for (index, mode) in [
        WirePermissionProfile::FullAccess,
        WirePermissionProfile::RequestApproval,
        WirePermissionProfile::Assisted,
    ]
    .into_iter()
    .enumerate()
    {
        let envelope = CommandEnvelope {
            command_id: CommandId::new(format!("desktop-mode-{index}")),
            session_id: session_id.clone(),
            expected_version: None,
            issued_at: leveler_core::now().to_rfc3339(),
            command: ClientCommand::SetPermissionProfile {
                session_id: session_id.clone(),
                mode,
            },
        };
        let mut foreign = envelope.clone();
        foreign.session_id = other.clone();
        assert!(client.deliver(foreign).await.is_err());
        client.deliver(envelope.clone()).await.unwrap();
        assert_eq!(client.snapshot(&session_id).await.unwrap().mode, mode);
        let event = tokio::time::timeout(std::time::Duration::from_secs(3), events.recv())
            .await
            .unwrap()
            .unwrap();
        assert!(
            matches!(event,RuntimeEvent::SessionUpdated{session} if session.id==session_id && session.mode==mode)
        );
        // A new client starts from a different fallback; the stored session
        // value still wins, proving this is not merely the first client's UI.
        let reopened = InProcessRuntimeClient::new(
            app.clone(),
            model.clone(),
            PermissionProfile::RequestApproval,
            false,
        );
        assert_eq!(reopened.snapshot(&session_id).await.unwrap().mode, mode);
        assert_eq!(client.snapshot(&other).await.unwrap().mode, other_before);
        assert_eq!(config_bytes(&config_path), global_before);
        client.deliver(envelope).await.unwrap();
        assert!(matches!(
            events.try_recv(),
            Err(tokio::sync::broadcast::error::TryRecvError::Empty)
        ));
    }
    assert_eq!(client.snapshot(&session_id).await.unwrap().mode, initial);
}
