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
use leveler_storage::{SessionRecord, SessionRepository};
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
        repo.update_status(
            &parent,
            SessionStatus::Running,
            AgentState::Execute,
            leveler_core::now(),
        )
        .await?;
        let log = EventLog::new(&db, parent.clone());
        let sink = &mut |_: EngineEvent| {};
        log.append(
            None,
            EngineEvent::TaskStarted {
                goal: task.to_string(),
                model: model.to_string(),
                mode: mode_str(mode).to_string(),
                sandbox: false,
                kind: ExecutionKind::Parallel,
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
                let result = app
                    .orchestrate_task(
                        &model,
                        mode,
                        &task,
                        Arc::new(AutoApprove),
                        Arc::new(leveler_agent::AutoClarify),
                        false,
                        &mut |_| {},
                        cancellation.clone(),
                        None, // one-shot parallel run: a fresh session per worker
                    )
                    .await
                    .ok();

                // Commit the candidate's changes (never .leveler/).
                let git = GitWorkflow::with_environment(&path, app.environment.clone());
                git.commit_changes("parallel candidate", &cancellation)
                    .await
                    .ok()?;

                let (child_session, verified) = match result {
                    Some((session_id, report)) => {
                        (session_id.to_string(), report.outcome.is_success())
                    }
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
        repo.set_outcome(&parent, outcome, leveler_core::now())
            .await?;
        // Operational status only; the verified-vs-unverified verdict is the
        // `outcome` column above.
        let (status, state) = match outcome {
            TaskOutcome::Verified => (SessionStatus::Completed, AgentState::Complete),
            TaskOutcome::CompletedUnverified => (SessionStatus::Completed, AgentState::Complete),
            _ => (SessionStatus::Failed, AgentState::Failed),
        };
        repo.update_status(&parent, status, state, leveler_core::now())
            .await?;
        log.append(
            None,
            EngineEvent::TaskFinished {
                outcome,
                reason: (outcome != TaskOutcome::Verified).then(|| {
                    format!(
                        "{} candidate(s), {} verified, {} integrated",
                        candidates.len(),
                        verified,
                        merge.integrated.len()
                    )
                }),
            },
            sink,
        )
        .await
        .map_err(crate::session::app_error_from_engine)?;

        Ok(ParallelEditOutcome {
            candidates: candidates.len(),
            verified,
            integrated: merge.integrated,
            conflicted: merge.conflicted,
            session: parent.to_string(),
        })
    }
}
