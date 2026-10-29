//! Parallel multi-agent editing (spec §42): run N agents concurrently on the
//! same task, each in an isolated git worktree, then integrate their candidate
//! branches — union of disjoint edits, verified-wins on same-region conflicts.

use std::sync::Arc;

use tokio_util::sync::CancellationToken;

use leveler_agent::coding::mode_str;
use leveler_engine::{EngineEvent, EventLog, ExecutionKind, NewSession, TaskOutcome};
use leveler_execution::{Approver, PermissionProfile};
use leveler_lifecycle::{AgentState, SessionStatus};
use leveler_model::ModelRef;
use leveler_project::Layout;
use leveler_vcs::{GitWorkflow, MergeCandidate, slugify, worktree_path};

use crate::{AppError, Application};

/// The result of a parallel edit.
#[derive(Debug, Clone, Default)]
pub struct ParallelEditOutcome {
    pub candidates: usize,
    pub verified: usize,
    pub integrated: Vec<String>,
    pub conflicted: Vec<String>,
    /// Cleanup failures; integration and the primary task outcome remain authoritative.
    pub warnings: Vec<String>,
    /// The parent session recording the run (kind=parallel; its event log
    /// references every candidate's child session).
    pub session: String,
}

fn honor_parent_cancellation(
    result: Result<ParallelEditOutcome, AppError>,
    cancellation: &CancellationToken,
) -> Result<ParallelEditOutcome, AppError> {
    if cancellation.is_cancelled() {
        Err(AppError::Agent(leveler_agent::AgentError::Cancelled))
    } else {
        result
    }
}

async fn cleanup_worktrees(
    git: &GitWorkflow,
    worktrees: &[(std::path::PathBuf, String)],
) -> Vec<String> {
    let cancellation = CancellationToken::new();
    let mut warnings = Vec::new();
    for (path, branch) in worktrees {
        if let Err(error) = git.remove_worktree(path, branch, &cancellation).await {
            let warning = format!(
                "Failed to clean up worktree {} (branch {branch}): {error}",
                path.display()
            );
            tracing::warn!(%warning);
            warnings.push(warning);
        }
    }
    warnings
}

impl Application {
    /// Run `n` agents concurrently on `task` in isolated worktrees and integrate
    /// the results into the current branch. Requires a clean, committed repo.
    ///
    /// Only `goal` is accepted: the pipeline selects on committed candidate
    /// edits and integrates them with `git merge`, while `chat` may end with a
    /// textual answer and `plan` is a read-only overlay — neither is guaranteed
    /// to produce the integrable change this path requires. The combination is
    /// refused here, before a parent session, a worktree, or a provider request.
    #[allow(clippy::too_many_arguments)]
    pub async fn parallel_edit(
        &self,
        model: &ModelRef,
        mode: PermissionProfile,
        task: &str,
        n: usize,
        approver: Arc<dyn Approver>,
        sandbox: bool,
        cancellation: CancellationToken,
    ) -> Result<ParallelEditOutcome, AppError> {
        if self.collaboration() != leveler_lifecycle::CollaborationMode::Goal {
            return Err(AppError::UnsupportedCombination(format!(
                "--parallel currently requires collaboration=goal (got `{}`): `chat` may end with a textual answer and `plan` is read-only, so neither is guaranteed to produce the integrable changes parallel integration selects on. Use `--collaboration goal`, or drop `--parallel`.",
                self.collaboration().as_str()
            )));
        }
        let n = n.max(2);
        let repo_root = self
            .layout
            .require_workspace()
            .map_err(|error| AppError::NotFound(error.to_string()))?
            .to_path_buf();
        let config_dir = self.layout.config_dir.clone();
        let main_git = GitWorkflow::with_environment(&repo_root, self.environment.clone());

        if main_git.has_changes(&cancellation).await? {
            return Err(AppError::NotFound(
                "parallel editing needs a clean working tree (commit or stash first)".into(),
            ));
        }
        let base = main_git.head_sha(&cancellation).await?;
        let slug = slugify(task);

        // The parent session (plan B9): kind=parallel, its event log records
        // every worktree candidate and the child session that produced it.
        let db = self.open_database().await?;
        let engine = self.task_engine(&db)?;
        let parent = engine
            .create_task(&NewSession {
                workspace: Some(repo_root.display().to_string()),
                goal: task.to_string(),
                model: model.to_string(),
                mode: mode_str(mode).to_string(),
                sandbox,
                kind: ExecutionKind::Parallel,
                axes: Some(leveler_engine::NewSessionAxes {
                    collaboration: self.collaboration().as_str().to_string(),
                }),
            })
            .await
            .map_err(crate::session::app_error_from_engine)?;
        let token = engine
            .mark_running(&parent, AgentState::Execute)
            .await
            .map_err(crate::session::app_error_from_engine)?;
        let log = EventLog::new_owned(&db, parent.clone(), token.clone());
        let sink = &mut |_: EngineEvent| {};
        // Once the parent is Running, every exit is converted into one
        // canonical terminal fact below. The body may fail at any worktree,
        // candidate-log, or integration step without stranding the parent.
        let mut worktrees = Vec::new();
        let result: Result<ParallelEditOutcome, AppError> = async {
            log.append(
                None,
                EngineEvent::TaskStarted {
                    goal: task.to_string(),
                    model: model.to_string(),
                    mode: mode_str(mode).to_string(),
                    sandbox,
                    kind: ExecutionKind::Parallel,
                    task_id: Some(token.task_id.clone()),
                },
                sink,
            )
            .await
            .map_err(crate::session::app_error_from_engine)?;

            // Create N isolated worktrees off the base commit.
            for i in 0..n {
                let path = worktree_path(&slug, i);
                let branch = format!("leveler/parallel-{slug}-{parent}-{i}");
                main_git
                    .add_worktree(&path, &branch, &base, &cancellation)
                    .await?;
                worktrees.push((path, branch));
            }

            for (_, branch) in &worktrees {
                log.append(
                    None,
                    EngineEvent::CandidateStarted {
                        branch: branch.clone(),
                    },
                    sink,
                )
                .await
                .map_err(crate::session::app_error_from_engine)?;
            }

            // Run one agent per worktree, concurrently.
            let collaboration = self.collaboration();
            let futures = worktrees.iter().map(|(path, branch)| {
                let path = path.clone();
                let branch = branch.clone();
                let config_dir = config_dir.clone();
                let model = model.clone();
                let task = task.to_string();
                let approver = approver.clone();
                let model_step_ceiling = self.model_step_ceiling;
                let cancellation = cancellation.child_token();
                async move {
                    let mut child_session = String::new();
                    let result: Result<bool, AppError> = async {
                        let layout = Layout::ephemeral_with_environment(
                            path.clone(),
                            Some(config_dir),
                            self.layout.home().root(),
                            &self.environment,
                        );
                        let mut app =
                            Application::assemble(layout)?.with_collaboration(collaboration);
                        app.model_step_ceiling = model_step_ceiling;
                        let session_id = app.create_session_with_mode(&model, &task, mode).await?;
                        child_session = session_id.to_string();
                        let outcome = app
                            .run_in_session(
                                &session_id,
                                &model,
                                mode,
                                &task,
                                approver,
                                sandbox,
                                &mut |_| {},
                                cancellation.clone(),
                            )
                            .await;
                        // Settle admitted work even when its turn failed.
                        app.flush_memory_for_headless().await;
                        let outcome = outcome?;
                        let verified = matches!(
                            outcome.stop_reason,
                            leveler_agent::StopReason::Completed
                                | leveler_agent::StopReason::Answered
                        );
                        if verified {
                            let git = GitWorkflow::with_environment(&path, app.environment.clone());
                            git.commit_changes("parallel candidate", &cancellation)
                                .await?;
                        }
                        Ok(verified)
                    }
                    .await;
                    (branch, child_session, result)
                }
            });

            let mut candidates: Vec<MergeCandidate> = Vec::new();
            let mut first_error = None;
            for (branch, child_session, result) in futures::future::join_all(futures).await {
                let verified = matches!(&result, Ok(true));
                log.append(
                    None,
                    EngineEvent::CandidateFinished {
                        branch: branch.clone(),
                        session_id: child_session,
                        verified,
                    },
                    sink,
                )
                .await
                .map_err(crate::session::app_error_from_engine)?;
                match result {
                    Ok(true) => candidates.push(MergeCandidate { branch, verified }),
                    Ok(false) => {}
                    Err(error) => {
                        tracing::warn!(%branch, %error, "parallel candidate failed");
                        if first_error.is_none() {
                            first_error = Some(error);
                        }
                    }
                }
            }
            // Infrastructure/provider failures are not unverified candidates.
            // Preserve their typed cause and settle the parent as failed.
            if let Some(error) = first_error {
                return Err(error);
            }
            let verified = candidates.iter().filter(|c| c.verified).count();

            // Integrate into the main working tree.
            let merge = main_git.integrate(&candidates, &cancellation).await?;

            Ok(ParallelEditOutcome {
                candidates: n,
                verified,
                integrated: merge.integrated,
                conflicted: merge.conflicted,
                warnings: Vec::new(),
                session: parent.to_string(),
            })
        }
        .await;
        // Cleanup is owned by this run and must also execute after setup,
        // candidate, logging or integration failure, including cancellation.
        let warnings = cleanup_worktrees(&main_git, &worktrees).await;
        let mut result = honor_parent_cancellation(result, &cancellation);
        if let Ok(summary) = &mut result {
            summary.warnings = warnings.clone();
        }

        let terminal = match &result {
            Ok(summary) if !summary.integrated.is_empty() => leveler_engine::TaskTerminal {
                outcome: TaskOutcome::Completed,
                reason: None,
                failure: None,
                stop: None,
                status: SessionStatus::Completed,
                state: AgentState::Complete,
                goal: None,
                warnings: warnings.clone(),
            },
            Ok(summary) => leveler_engine::TaskTerminal {
                outcome: TaskOutcome::Failed,
                reason: Some(format!(
                    "{} candidate(s), {} verified, 0 integrated",
                    summary.candidates, summary.verified
                )),
                failure: None,
                stop: None,
                status: SessionStatus::Failed,
                state: AgentState::Failed,
                goal: None,
                warnings: warnings.clone(),
            },
            Err(AppError::Agent(leveler_agent::AgentError::Cancelled)) => {
                leveler_engine::TaskTerminal {
                    outcome: TaskOutcome::Interrupted,
                    reason: None,
                    failure: None,
                    stop: None,
                    status: SessionStatus::Interrupted,
                    state: AgentState::Execute,
                    goal: None,
                    warnings: warnings.clone(),
                }
            }
            Err(error) => leveler_engine::TaskTerminal {
                outcome: TaskOutcome::Failed,
                reason: Some(error.to_string()),
                failure: match error {
                    AppError::Model(model) => Some(model.clone()),
                    _ => None,
                },
                stop: None,
                status: SessionStatus::Failed,
                state: AgentState::Failed,
                goal: None,
                warnings: warnings.clone(),
            },
        };
        // Terminal event + every lifecycle column in ONE transaction — the
        // same barrier the engine uses. Three separate writes here used to
        // leave a window where a crash produced an outcome without its
        // canonical TaskFinished event (or vice versa).
        engine
            .finish_task(&token, &parent, terminal, sink)
            .await
            .map_err(crate::session::app_error_from_engine)?;

        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn cleanup_reports_retained_paths_and_still_cleans_other_worktrees() {
        use leveler_test_support::git::{run, scratch_repo};
        let repo = scratch_repo();
        run(repo.path(), &["commit", "--allow-empty", "-qm", "seed"]);
        let tmp = tempfile::tempdir().unwrap();
        let locked = tmp.path().join("locked");
        let removable = tmp.path().join("removable");
        for (path, branch) in [(&locked, "locked"), (&removable, "removable")] {
            run(
                repo.path(),
                &[
                    "worktree",
                    "add",
                    "-b",
                    branch,
                    path.to_str().unwrap(),
                    "HEAD",
                ],
            );
        }
        run(repo.path(), &["worktree", "lock", locked.to_str().unwrap()]);
        let warnings = cleanup_worktrees(
            &GitWorkflow::new(repo.path()),
            &[
                (locked.clone(), "locked".into()),
                (removable.clone(), "removable".into()),
            ],
        )
        .await;
        let retained = locked.exists();
        let removed = !removable.exists();
        run(
            repo.path(),
            &["worktree", "unlock", locked.to_str().unwrap()],
        );
        run(
            repo.path(),
            &["worktree", "remove", "--force", locked.to_str().unwrap()],
        );
        run(repo.path(), &["branch", "-D", "locked"]);
        assert!(retained && removed);
        assert_eq!(warnings.len(), 1, "cleanup failures must be reported");
        assert!(warnings[0].contains(locked.to_str().unwrap()));
        assert!(warnings[0].contains("locked"));
    }

    #[test]
    fn candidate_failure_preserves_cause_and_settles_every_started_candidate() {
        let home = tempfile::tempdir().unwrap();
        let status = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "parallel::tests::candidate_failure_child",
                "--nocapture",
            ])
            .env("LEVELER_TEST_PARALLEL_FAILURE", "1")
            .env("LEVELER_HOME", home.path())
            .status()
            .unwrap();
        assert!(status.success());
    }

    #[tokio::test]
    async fn candidate_failure_child() {
        if std::env::var_os("LEVELER_TEST_PARALLEL_FAILURE").is_none() {
            return;
        }

        use leveler_test_support::git::{run, scratch_repo};
        let repo = scratch_repo();
        std::fs::create_dir_all(repo.path().join(".leveler")).unwrap();
        std::fs::write(
            repo.path().join(".leveler/config.yaml"),
            "memory:\n  enabled: false\n",
        )
        .unwrap();
        run(repo.path(), &["add", "."]);
        run(repo.path(), &["commit", "-qm", "seed"]);
        let tmp = tempfile::tempdir().unwrap();
        let app = Application::assemble(Layout::from_parts(
            repo.path().to_path_buf(),
            tmp.path().join("configs"),
            tmp.path().join("state"),
        ))
        .unwrap();
        let error = app
            .parallel_edit(
                &ModelRef::new("unconfigured", "model"),
                PermissionProfile::Assisted,
                "failure-probe",
                2,
                Arc::new(leveler_execution::AutoApprove),
                false,
                CancellationToken::new(),
            )
            .await
            .expect_err("candidate errors must not become an empty success");
        assert!(error.to_string().contains("unconfigured"), "{error}");
        let db = app.open_database().await.unwrap();
        let parent = leveler_storage::SessionRepository::new(&db)
            .list()
            .await
            .unwrap()
            .into_iter()
            .find(|session| session.goal == "failure-probe")
            .unwrap();
        let id = leveler_core::SessionId::new(parent.id);
        let events = leveler_storage::EventRepository::new(&db)
            .load(&id)
            .await
            .unwrap();
        let finished: Vec<_> = events
            .iter()
            .filter_map(
                |record| match EngineEvent::from_payload(&record.payload).unwrap() {
                    EngineEvent::CandidateFinished {
                        session_id,
                        verified,
                        ..
                    } => Some((session_id, verified)),
                    _ => None,
                },
            )
            .collect();
        assert_eq!(finished.len(), 2);
        assert!(
            finished
                .iter()
                .all(|(id, verified)| !id.is_empty() && !verified)
        );
        let projects = app.layout.home().projects_dir();
        let mut child_sessions = Vec::new();
        if projects.exists() {
            for entry in std::fs::read_dir(projects).unwrap() {
                let database = entry.unwrap().path().join("sessions.db");
                if database.exists() {
                    let child_db = leveler_storage::Database::connect(&database).await.unwrap();
                    child_sessions.extend(
                        leveler_storage::SessionRepository::new(&child_db)
                            .list()
                            .await
                            .unwrap()
                            .into_iter()
                            .map(|row| row.id),
                    );
                }
            }
        }
        assert!(
            finished.iter().all(|(id, _)| child_sessions.contains(id)),
            "candidate sessions must remain under the parent's isolated home"
        );
        assert_eq!(parent.status, SessionStatus::Failed);
        let output = std::process::Command::new("git")
            .args(["worktree", "list", "--porcelain"])
            .current_dir(repo.path())
            .output()
            .unwrap();
        assert!(output.status.success());
        assert_eq!(
            String::from_utf8(output.stdout)
                .unwrap()
                .lines()
                .filter(|line| line.starts_with("worktree "))
                .count(),
            1
        );
    }

    #[test]
    fn parent_cancellation_cannot_become_a_successful_empty_result() {
        let cancellation = CancellationToken::new();
        cancellation.cancel();
        let result = honor_parent_cancellation(Ok(ParallelEditOutcome::default()), &cancellation);
        assert!(matches!(
            result,
            Err(AppError::Agent(leveler_agent::AgentError::Cancelled))
        ));
    }

    /// `chat` and `plan` cannot guarantee a committed, integrable candidate
    /// edit, so the refusal must land at the entry — before the layout is even
    /// asked for a workspace, let alone a session or a provider request.
    #[tokio::test]
    async fn parallel_edit_refuses_axes_that_cannot_integrate() {
        for axis in [
            leveler_lifecycle::CollaborationMode::Chat,
            leveler_lifecycle::CollaborationMode::Plan,
        ] {
            let tmp = tempfile::tempdir().unwrap();
            let layout = Layout::from_parts(
                tmp.path().to_path_buf(),
                tmp.path().join("configs"),
                tmp.path().join("state"),
            );
            let app = Application::assemble(layout)
                .unwrap()
                .with_collaboration(axis);
            // The layout has no workspace on purpose: reaching that check
            // would mean the contract was validated after the fail-fast point.
            let error = app
                .parallel_edit(
                    &ModelRef::new("mock", "m"),
                    PermissionProfile::Assisted,
                    "task",
                    3,
                    Arc::new(leveler_execution::AutoApprove),
                    false,
                    CancellationToken::new(),
                )
                .await
                .expect_err("a non-goal axis must be refused at the entry");
            assert!(matches!(&error, AppError::UnsupportedCombination(_)));
            let message = error.to_string();
            for needle in ["--parallel", "collaboration=goal", axis.as_str()] {
                assert!(message.contains(needle), "{needle} missing: {message}");
            }
        }
    }
}
