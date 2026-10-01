//! A read projection over existing session stores; never an execution authority.
use leveler_client_protocol::{
    ClientError, UiGlobalTaskIndex, UiGlobalTaskSourceError, UiGlobalTaskSourceErrorKind,
    UiGlobalTaskSummary,
};
use leveler_core::{LevelerHome, SessionId};
use leveler_storage::Database;
use std::path::{Path, PathBuf};

/// Discover actual state databases, retaining historical stores whose checkout disappeared.
fn sources(home: &LevelerHome) -> (Vec<PathBuf>, Vec<UiGlobalTaskSourceError>) {
    let mut paths = Vec::new();
    let mut errors = Vec::new();
    let directory = home.projects_dir();
    match std::fs::read_dir(&directory) {
        Ok(entries) => {
            for entry in entries {
                match entry {
                    Ok(entry) => {
                        let db = entry.path().join("sessions.db");
                        add_source(db, &mut paths, &mut errors);
                    }
                    Err(error) => errors.push(source_error(
                        &directory,
                        UiGlobalTaskSourceErrorKind::Discovery,
                        error,
                    )),
                }
            }
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => errors.push(source_error(
            &directory,
            UiGlobalTaskSourceErrorKind::Discovery,
            error,
        )),
    }
    let no_workspace = home.no_workspace_state_dir().join("sessions.db");
    add_source(no_workspace, &mut paths, &mut errors);
    paths.sort();
    paths.dedup();
    (paths, errors)
}
fn add_source(path: PathBuf, paths: &mut Vec<PathBuf>, errors: &mut Vec<UiGlobalTaskSourceError>) {
    match std::fs::metadata(&path) {
        Ok(metadata) if metadata.is_file() => paths.push(path),
        Ok(_) => errors.push(source_error(
            &path,
            UiGlobalTaskSourceErrorKind::Discovery,
            "session store is not a regular file",
        )),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => errors.push(source_error(
            &path,
            UiGlobalTaskSourceErrorKind::Discovery,
            error,
        )),
    }
}
fn source_error(
    path: &Path,
    kind: UiGlobalTaskSourceErrorKind,
    error: impl std::fmt::Display,
) -> UiGlobalTaskSourceError {
    UiGlobalTaskSourceError {
        source_id: path.display().to_string(),
        kind,
        message: error.to_string(),
    }
}

/// Read the global directory without starting daemons, creating stores or migrating them.
pub async fn query_global_tasks(home: &LevelerHome, include_archived: bool) -> UiGlobalTaskIndex {
    // Each store owns its boot-lock namespace. A probe for the requesting
    // runtime cannot establish another source's boot liveness.
    let (paths, source_errors) = sources(home);
    let mut index = UiGlobalTaskIndex {
        tasks: Vec::new(),
        source_errors,
    };
    for path in paths {
        let loaded = async {
            let db = Database::connect_read_only(&path).await?;
            db.session_facts(include_archived).await
        }
        .await;
        let facts = match loaded {
            Ok(facts) => facts,
            Err(error) => {
                index.source_errors.push(source_error(
                    &path,
                    UiGlobalTaskSourceErrorKind::Storage,
                    error,
                ));
                continue;
            }
        };
        let probe = crate::runtime_boot::StateDirBootLiveness::new(
            path.parent().expect("discovered database has a parent"),
        );
        for facts in facts {
            let (status, _) =
                match crate::session_projection::project_task(&facts, &probe, false, false) {
                    Ok(projection) => projection,
                    Err(error) => {
                        index.source_errors.push(source_error(
                            &path,
                            UiGlobalTaskSourceErrorKind::Projection,
                            error,
                        ));
                        continue;
                    }
                };
            index.tasks.push(UiGlobalTaskSummary {
                id: SessionId::new(facts.session.id),
                source_id: path.display().to_string(),
                title: crate::session_projection::project_title(
                    &facts.session.goal,
                    facts.first_user_text.as_deref(),
                ),
                workspace_available: facts
                    .session
                    .repository
                    .as_deref()
                    .map(|workspace| Path::new(workspace).is_dir()),
                primary_workspace: facts.session.repository,
                last_activity_at: facts.last_activity_at,
                archived_at: facts.archived_at,
                status,
                model: facts.session.model,
            });
        }
    }
    index.tasks.sort_by(|a, b| {
        b.last_activity_at
            .cmp(&a.last_activity_at)
            .then_with(|| a.source_id.cmp(&b.source_id))
            .then_with(|| a.id.as_str().cmp(b.id.as_str()))
    });
    index
}

/// Resolve a source-aware identity against discovered stores, rejecting arbitrary paths.
/// This gives a caller the existing state store and optional workspace; runtime
/// creation/connection remains the runtime host's responsibility.
pub async fn resolve_global_task_source(
    home: &LevelerHome,
    source_id: &str,
    session_id: &SessionId,
) -> Result<(PathBuf, Option<String>), ClientError> {
    let (paths, _) = sources(home);
    let path = paths
        .into_iter()
        .find(|path| path.to_string_lossy() == source_id)
        .ok_or_else(|| ClientError::Runtime("unknown task source".into()))?;
    let db = Database::connect_read_only(&path)
        .await
        .map_err(|e| ClientError::Runtime(e.to_string()))?;
    let row = leveler_storage::SessionRepository::new(&db)
        .get(session_id)
        .await
        .map_err(|e| ClientError::Runtime(e.to_string()))?
        .ok_or_else(|| ClientError::SessionNotFound(session_id.clone()))?;
    Ok((path, row.repository))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn persisted_assistant_final_and_successful_tool_do_not_complete_a_task() {
        let tmp = tempfile::tempdir().unwrap();
        let home = LevelerHome::from_root(tmp.path());
        std::fs::create_dir_all(home.no_workspace_state_dir()).unwrap();
        let db = Database::connect(&home.no_workspace_state_dir().join("sessions.db"))
            .await
            .unwrap();
        let record = leveler_storage::SessionRecord::without_workspace(
            "finish a task",
            "m",
            leveler_core::now(),
        );
        let id = SessionId::new(record.id.clone());
        let sessions = leveler_storage::SessionRepository::new(&db);
        sessions.create(&record).await.unwrap();
        sessions
            .update_status(
                &id,
                leveler_lifecycle::SessionStatus::Running,
                leveler_lifecycle::AgentState::Execute,
                leveler_core::now(),
            )
            .await
            .unwrap();
        let turns = leveler_storage::TurnRepository::new(&db);
        let turn = turns
            .start(&id, "chat", None, leveler_core::now())
            .await
            .unwrap();
        let turn_id = leveler_core::TurnId::new(turn.id);
        let assistant = leveler_model::Message::text(
            leveler_model::Role::Assistant,
            "All done. The tests pass and the task is complete.",
        );
        leveler_storage::MessageRepository::new(&db)
            .append(
                &id,
                &[serde_json::to_string(&assistant).unwrap()],
                leveler_core::now(),
            )
            .await
            .unwrap();
        let tool = leveler_engine::EngineEvent::ToolCallFinished {
            call_id: "successful-test".into(),
            name: "run_command".into(),
            is_error: false,
            preview: "test result: ok. 1 passed".into(),
            agent_id: None,
            applied_diff: None,
            exit_code: Some(0),
            stop: None,
        };
        let events = leveler_storage::EventRepository::new(&db);
        let (tag, payload) = tool.to_row().unwrap();
        events
            .append(&id, Some(&turn_id), &tag, &payload, leveler_core::now())
            .await
            .unwrap();
        turns
            .finish(&turn_id, "completed", leveler_core::now())
            .await
            .unwrap();
        let before = query_global_tasks(&home, false).await;
        assert!(before.source_errors.is_empty());
        assert_eq!(before.tasks.len(), 1);
        assert_ne!(
            before.tasks[0].status,
            leveler_client_protocol::UiTaskStatus::Completed,
            "persisted final prose, exit 0 and successful tests are not TaskFinished"
        );
        assert!(
            db.session_facts_for(&id)
                .await
                .unwrap()
                .unwrap()
                .terminal
                .is_none()
        );
        let terminal = leveler_engine::EngineEvent::TaskFinished {
            outcome: leveler_lifecycle::TaskOutcome::Completed,
            stop: Some(leveler_lifecycle::StopReason::Completed),
            reason: None,
            failure: None,
            warnings: Vec::new(),
        };
        let (tag, payload) = terminal.to_row().unwrap();
        events
            .append(&id, None, &tag, &payload, leveler_core::now())
            .await
            .unwrap();
        let after = query_global_tasks(&home, false).await;
        assert!(after.source_errors.is_empty());
        assert_eq!(
            after.tasks[0].status,
            leveler_client_protocol::UiTaskStatus::Completed
        );
    }
    #[cfg(unix)]
    #[tokio::test]
    async fn permission_denied_source_keeps_other_tasks_and_reports_discovery_error() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().unwrap();
        let home = LevelerHome::from_root(tmp.path());
        for namespace in ["visible", "denied"] {
            let dir = home.project_state_dir(namespace);
            std::fs::create_dir_all(&dir).unwrap();
            let db = Database::connect(&dir.join("sessions.db")).await.unwrap();
            let row = leveler_storage::SessionRecord::new(
                "/checkout",
                namespace,
                "m",
                leveler_core::now(),
            );
            leveler_storage::SessionRepository::new(&db)
                .create(&row)
                .await
                .unwrap();
        }
        let denied = home.project_state_dir("denied");
        std::fs::set_permissions(&denied, std::fs::Permissions::from_mode(0o000)).unwrap();
        let actual_denial = std::fs::metadata(denied.join("sessions.db")).unwrap_err();
        let index = query_global_tasks(&home, false).await;
        std::fs::set_permissions(&denied, std::fs::Permissions::from_mode(0o700)).unwrap();
        assert_eq!(actual_denial.kind(), std::io::ErrorKind::PermissionDenied);
        assert_eq!(index.tasks.len(), 1);
        assert_eq!(index.tasks[0].title, "visible");
        assert_eq!(index.source_errors.len(), 1);
        assert_eq!(
            index.source_errors[0].kind,
            UiGlobalTaskSourceErrorKind::Discovery
        );
    }
    #[cfg(unix)]
    #[tokio::test]
    async fn discovery_metadata_error_is_not_empty_success() {
        let tmp = tempfile::tempdir().unwrap();
        let home = LevelerHome::from_root(tmp.path());
        for dir in [
            home.project_state_dir("broken"),
            home.no_workspace_state_dir(),
        ] {
            std::fs::create_dir_all(&dir).unwrap();
            std::os::unix::fs::symlink("sessions.db", dir.join("sessions.db")).unwrap();
        }
        let index = query_global_tasks(&home, false).await;
        assert_eq!(
            index.source_errors.len(),
            2,
            "unreadable metadata must not become an empty successful index"
        );
        assert!(
            index
                .source_errors
                .iter()
                .all(|error| error.kind == UiGlobalTaskSourceErrorKind::Discovery)
        );
    }

    #[tokio::test]
    async fn offline_directory_lists_all_workspaces_and_partial_source_failure() {
        let tmp = tempfile::tempdir().unwrap();
        let home = LevelerHome::from_root(tmp.path());
        let mut ids = Vec::new();
        for (namespace, workspace) in [
            ("repo-a", Some("/deleted/a")),
            ("repo-b", Some("/b")),
            ("", None),
        ] {
            let dir = if namespace.is_empty() {
                home.no_workspace_state_dir()
            } else {
                home.project_state_dir(namespace)
            };
            std::fs::create_dir_all(&dir).unwrap();
            let db = Database::connect(&dir.join("sessions.db")).await.unwrap();
            let row = match workspace {
                Some(path) => {
                    leveler_storage::SessionRecord::new(path, "title", "m", leveler_core::now())
                }
                None => leveler_storage::SessionRecord::without_workspace(
                    "title",
                    "m",
                    leveler_core::now(),
                ),
            };
            leveler_storage::SessionRepository::new(&db)
                .create(&row)
                .await
                .unwrap();
            ids.push(row.id);
        }
        let index = query_global_tasks(&home, false).await;
        assert!(index.source_errors.is_empty());
        assert_eq!(index.tasks.len(), 3);
        for id in &ids {
            assert!(index.tasks.iter().any(|row| row.id.as_str() == id.as_str()));
        }
        assert_eq!(
            index
                .tasks
                .iter()
                .filter(|row| row.primary_workspace.is_none())
                .count(),
            1
        );
        // Sidebar metadata mutations use the existing per-source repository.
        let db = Database::connect(&home.project_state_dir("repo-a").join("sessions.db"))
            .await
            .unwrap();
        let id = SessionId::new(ids[0].clone());
        let sessions = leveler_storage::SessionRepository::new(&db);
        sessions.update_goal(&id, "renamed").await.unwrap();
        sessions
            .set_archived(&id, Some(leveler_core::now()))
            .await
            .unwrap();
        let visible = query_global_tasks(&home, false).await;
        assert_eq!(visible.tasks.len(), 2);
        let archived = query_global_tasks(&home, true).await;
        let task = archived.tasks.iter().find(|task| task.id == id).unwrap();
        assert_eq!(task.title, "renamed");
        assert!(task.archived_at.is_some());
        assert_eq!(task.primary_workspace.as_deref(), Some("/deleted/a"));
        assert_eq!(task.workspace_available, Some(false));
        assert!(!task.last_activity_at.is_empty());
        sessions.set_archived(&id, None).await.unwrap();
        let event = leveler_engine::EngineEvent::TaskFinished {
            outcome: leveler_lifecycle::TaskOutcome::Completed,
            reason: None,
            stop: Some(leveler_lifecycle::StopReason::Answered),
            failure: None,
            warnings: Vec::new(),
        };
        let (tag, payload) = event.to_row().unwrap();
        leveler_storage::EventRepository::new(&db)
            .append(&id, None, &tag, &payload, leveler_core::now())
            .await
            .unwrap();
        let answered = query_global_tasks(&home, false).await;
        assert_eq!(
            answered
                .tasks
                .iter()
                .find(|task| task.id == id)
                .unwrap()
                .status,
            leveler_client_protocol::UiTaskStatus::Answered
        );
        let dir = home.project_state_dir("corrupt");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("sessions.db"), "corrupt").unwrap();
        let index = query_global_tasks(&home, false).await;
        assert_eq!(index.tasks.len(), 3);
        assert_eq!(index.source_errors.len(), 1);
        assert_eq!(
            index.source_errors[0].kind,
            UiGlobalTaskSourceErrorKind::Storage
        );
        let task = &index.tasks[0];
        assert!(
            resolve_global_task_source(&home, &task.source_id, &task.id)
                .await
                .is_ok()
        );
        assert!(
            resolve_global_task_source(&home, "/tmp/arbitrary.db", &task.id)
                .await
                .is_err()
        );
    }
}
