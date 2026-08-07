//! Parallel multi-agent editing (spec §42): run N agents concurrently on the
//! same task, each in an isolated git worktree, then integrate their candidate
//! branches — union of disjoint edits, verified-wins on same-region conflicts.

use std::sync::Arc;

use tokio_util::sync::CancellationToken;

use leveler_engine::{EngineEvent, EventLog, ExecutionKind, TaskOutcome, mode_str};
use leveler_execution::{AutoApprove, PermissionProfile};
use leveler_lifecycle::{AgentState, SessionStatus};
use leveler_model::ModelRef;
use leveler_project::Layout;
use leveler_storage::{SessionRecord, SessionRepository, TaskStore};
use leveler_vcs::{GitWorkflow, MergeCandidate, slugify, worktree_path};

use crate::{AppError, Application};

/// The result of a parallel edit.
#[derive(Debug, Clone, Default)]
pub struct ParallelEditOutcome {
    pub candidates: usize,
    pub verified: usize,
    pub integrated: Vec<String>,
    pub conflicted: Vec<String>,
    /// The parent session recording the run (kind=parallel; its event log
    /// references every candidate's child session).
    pub session: String,
}

impl Application {
    /// Run `n` agents concurrently on `task` in isolated worktrees and integrate
    /// the results into the current branch. Requires a clean, committed repo.
    pub async fn parallel_edit(
        &self,
        model: &ModelRef,
        mode: PermissionProfile,
        task: &str,
        n: usize,
        cancellation: CancellationToken,
    ) -> Result<ParallelEditOutcome, AppError> {
        let n = n.max(2);
        let repo_root = self.layout.repo_root.clone();
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
        let record = SessionRecord::new(
            repo_root.display().to_string(),
            task,
            model.to_string(),
            leveler_core::now(),
        );
        let repo = SessionRepository::new(&db);
        repo.create(&record).await?;
        let parent = leveler_core::SessionId::new(record.id);
        repo.set_execution(
            &parent,
            mode_str(mode),
            false,
            ExecutionKind::Parallel.as_str(),
            leveler_core::now(),
        )
        .await?;
        // The parallel parent never enters the engine's run path, so its
        // durable task row, ownership acquisition, and fenced Running
        // transition happen here (the engine does the same in `mark_running`
        // for direct/chat/resume sessions). Everything canonical below rides
        // this token; if ownership is lost mid-run, the writes fail typed —
        // a stale runtime never stamps a covering terminal fact.
        let task_id = TaskStore::ensure_for_session(&db, &parent, leveler_core::now()).await?;
        let runtime_id = self.runtime_id()?;
        let current = leveler_storage::OwnershipStore::current(&db, &task_id)
            .await?
            .ok_or_else(|| AppError::NotFound(format!("no task row for session {parent}")))?;
        let token =
            leveler_storage::OwnershipStore::acquire(&db, &task_id, &runtime_id, current.epoch)
                .await
                .map_err(|e| AppError::Engine(e.to_string()))?;
        leveler_storage::SessionStore::update_status_owned(
            &db,
            &token,
            &parent,
            SessionStatus::Running,
            AgentState::Execute,
            leveler_core::now(),
        )
        .await
        .map_err(|e| AppError::Engine(e.to_string()))?;
        let log = EventLog::new_owned(&db, parent.clone(), token.clone());
        let sink = &mut |_: EngineEvent| {};
        log.append(
            None,
            EngineEvent::TaskStarted {
                goal: task.to_string(),
                model: model.to_string(),
                mode: mode_str(mode).to_string(),
                sandbox: false,
                kind: ExecutionKind::Parallel,
                task_id: Some(token.task_id.clone()),
            },
            sink,
        )
        .await
        .map_err(crate::session::app_error_from_engine)?;

        // Create N isolated worktrees off the base commit.
        let mut worktrees = Vec::new();
        for i in 0..n {
            let path = worktree_path(&slug, i);
            let branch = format!("leveler/parallel-{slug}-{i}");
            let _ = std::fs::remove_dir_all(&path);
            // Clean up a stale branch from a prior run.
            let _ = main_git
                .remove_worktree(&path, &branch, &cancellation)
                .await;
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
        let futures = worktrees.iter().map(|(path, branch)| {
            let path = path.clone();
            let branch = branch.clone();
            let config_dir = config_dir.clone();
            let model = model.clone();
            let task = task.to_string();
            let cancellation = cancellation.child_token();
            async move {
                let layout = Layout::resolve(path.clone(), Some(config_dir));
                let app = Application::assemble(layout).ok()?;
                // Direct tool loop only — no orchestrate dual path.
                let result = async {
                    let session_id = app.create_session(&model, &task).await.ok()?;
                    let outcome = app
                        .run_in_session(
                            &session_id,
                            &model,
                            mode,
                            &task,
                            Arc::new(AutoApprove),
                            false,
                            &mut |_| {},
                            cancellation.clone(),
                        )
                        .await
                        .ok()?;
                    Some((session_id, outcome))
                }
                .await;

                // Commit the candidate's changes (never .leveler/).
                let git = GitWorkflow::with_environment(&path, app.environment.clone());
                git.commit_changes("parallel candidate", &cancellation)
                    .await
                    .ok()?;

                let (child_session, verified) = match result {
                    Some((session_id, outcome)) => (
                        session_id.to_string(),
                        matches!(
                            outcome.stop_reason,
                            leveler_agent::StopReason::Completed
                                | leveler_agent::StopReason::Answered
                        ),
                    ),
                    None => (String::new(), false),
                };
                Some((MergeCandidate { branch, verified }, child_session))
            }
        });

        let mut candidates: Vec<MergeCandidate> = Vec::new();
        for (candidate, child_session) in futures::future::join_all(futures)
            .await
            .into_iter()
            .flatten()
        {
            log.append(
                None,
                EngineEvent::CandidateFinished {
                    branch: candidate.branch.clone(),
                    session_id: child_session,
                    verified: candidate.verified,
                },
                sink,
            )
            .await
            .map_err(crate::session::app_error_from_engine)?;
            if candidate.verified {
                candidates.push(candidate);
            }
        }
        let verified = candidates.iter().filter(|c| c.verified).count();

        // Integrate into the main working tree.
        let merge = main_git.integrate(&candidates, &cancellation).await?;

        // Clean up worktrees and their branches.
        for (path, branch) in &worktrees {
            main_git.remove_worktree(path, branch, &cancellation).await;
        }

        // Terminal outcome (阶段A semantics): integrated + at least one
        // verified candidate → Verified; integrated only → unverified; no
        // integration → Failed.
        let outcome = if !merge.integrated.is_empty() && verified > 0 {
            TaskOutcome::Verified
        } else if !merge.integrated.is_empty() {
            TaskOutcome::CompletedUnverified
        } else {
            TaskOutcome::Failed
        };
        // Operational status; the verified-vs-unverified verdict is `outcome`.
        let (status, state) = match outcome {
            TaskOutcome::Verified => (SessionStatus::Completed, AgentState::Complete),
            TaskOutcome::CompletedUnverified => (SessionStatus::Completed, AgentState::Complete),
            _ => (SessionStatus::Failed, AgentState::Failed),
        };
        // Terminal event + every lifecycle column in ONE transaction — the
        // same barrier the engine uses. Three separate writes here used to
        // leave a window where a crash produced an outcome without its
        // canonical TaskFinished event (or vice versa).
        let event = EngineEvent::TaskFinished {
            stop: None,
            outcome,
            reason: (outcome != TaskOutcome::Verified).then(|| {
                format!(
                    "{} candidate(s), {} verified, {} integrated",
                    candidates.len(),
                    verified,
                    merge.integrated.len()
                )
            }),
        };
        let (event_type, payload) = event
            .to_row()
            .map_err(crate::session::app_error_from_engine)?;
        leveler_storage::TerminalStore::finish_task_owned(
            &db,
            &token,
            &parent,
            &event_type,
            &payload,
            outcome,
            status,
            state,
            leveler_core::now(),
        )
        .await
        .map_err(|e| AppError::Engine(e.to_string()))?;

        Ok(ParallelEditOutcome {
            candidates: candidates.len(),
            verified,
            integrated: merge.integrated,
            conflicted: merge.conflicted,
            session: parent.to_string(),
        })
    }
}
