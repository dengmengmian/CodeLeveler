//! Orchestrated task flow: the explicit state machine behind `leveler plan` and
//! `leveler run --orchestrate`.

use std::sync::Arc;

use tokio_util::sync::CancellationToken;

use leveler_engine::{EngineEvent, ExecutionKind, TaskOutcome, TaskReport, TaskSpec, mode_str};
use leveler_execution::{Approver, PermissionProfile, Workspace};
use leveler_lifecycle::{AgentState, SessionStatus};
use leveler_model::{ModelRef, ModelRuntime};
use leveler_orchestrator::{
    Discussion, DiscussionEvent, DiscussionOutcome, Orchestrator, OrchestratorEvent, Requirement,
    TaskGraph,
};
use leveler_storage::SessionRepository;
use leveler_tools::ToolContext;
use leveler_verifier::VerificationPlan;

use crate::{AppError, Application};

/// The verification plan for `root`, as the repository stands right now.
///
/// One definition, owned by the verifier: the post-edit gate re-reads it at
/// verification time (see `leveler_engine`'s `gate_plan`), so a turn that
/// creates the project is still verified.
pub(crate) fn verification_plan_for_root(root: &std::path::Path) -> VerificationPlan {
    leveler_verifier::discover::plan_for_repo(root)
}

impl Application {
    /// Build a read-only orchestrator for planning (`leveler plan`). Execution
    /// runs through the task engine — see [`Application::orchestrate_task`].
    async fn orchestrator_for(
        &self,
        model: &ModelRef,
        mode: PermissionProfile,
        sandbox: bool,
    ) -> Result<Orchestrator, AppError> {
        let workspace = Workspace::new(&self.layout.repo_root)?
            .with_readonly_roots(self.readonly_roots.iter().cloned());
        // Same tool-context limits as the engine path: resolver defaults (or
        // the eval override seam), not a model-bound policy.
        let (max_files, read_guard) =
            leveler_engine::resolve_tool_limits(self.execution_overrides.as_ref());
        let tool_context = ToolContext::with_environment(workspace, mode, self.environment.clone())
            .with_policy_limits(max_files, read_guard)
            .with_sandbox(sandbox)
            .with_auto_format(true)
            .with_deny_env(crate::provider_secret_env_names(&self.config.providers))
            .with_artifact_store(std::sync::Arc::new(leveler_execution::ArtifactStore::new(
                self.layout.state_dir.join("artifacts"),
            )))
            .with_memory_root(self.layout.memory_dir())
            // plan_task always passes RequestApproval historically; force Safe-only.
            .with_read_only(true);
        let registry = Arc::new(match self.work_profile() {
            leveler_agent::WorkProfile::Economy => leveler_tools::core_registry(),
            leveler_agent::WorkProfile::Balanced | leveler_agent::WorkProfile::Delivery => {
                leveler_tools::full_registry()
            }
        });
        let runtime: Arc<dyn ModelRuntime> = self.registry.clone();
        Ok(Orchestrator::new(
            runtime,
            registry,
            tool_context,
            model.clone(),
        ))
    }

    /// Run a multi-agent free discussion on a topic (spec §42).
    pub async fn discuss(
        &self,
        model: &ModelRef,
        topic: &str,
        rounds: u32,
        observer: &mut dyn FnMut(DiscussionEvent),
        cancellation: CancellationToken,
    ) -> Result<DiscussionOutcome, AppError> {
        let runtime: Arc<dyn ModelRuntime> = self.registry.clone();
        let discussion = Discussion::new(runtime, model.clone()).with_rounds(rounds);
        Ok(discussion.run(topic, observer, &cancellation).await?)
    }

    /// Read-only planning: Understand → Localize → Plan. No edits are made.
    pub async fn plan_task(
        &self,
        model: &ModelRef,
        goal: &str,
        observer: &mut dyn FnMut(OrchestratorEvent),
        cancellation: CancellationToken,
    ) -> Result<(Requirement, TaskGraph), AppError> {
        // Planning only reads the repo (no run_command); sandbox is irrelevant.
        let orchestrator = self
            .orchestrator_for(model, PermissionProfile::Assisted, false)
            .await?;
        let result = orchestrator
            .plan_only(goal, observer, &cancellation)
            .await?;
        Ok(result)
    }

    /// Run a task through the plan strategy on the task engine: every node is
    /// a persisted turn, strategy progress lands in the event log, and the
    /// terminal outcome is stamped on the session row.
    #[allow(clippy::too_many_arguments)]
    pub async fn orchestrate_task(
        &self,
        model: &ModelRef,
        mode: PermissionProfile,
        goal: &str,
        approver: Arc<dyn Approver>,
        clarifier: Arc<dyn leveler_agent::Clarifier>,
        sandbox: bool,
        observer: &mut dyn FnMut(EngineEvent),
        cancellation: tokio_util::sync::CancellationToken,
        existing_session: Option<leveler_core::SessionId>,
    ) -> Result<(leveler_core::SessionId, TaskReport), AppError> {
        self.orchestrate_task_with_policy(
            model,
            mode,
            goal,
            approver,
            clarifier,
            sandbox,
            observer,
            cancellation,
            existing_session,
            leveler_agent::ContinuationPolicy::UntilTerminal,
            self.top_level_limits(),
        )
        .await
    }

    /// Eval-only orchestrated entry point with a case-wide round budget.
    #[allow(clippy::too_many_arguments)]
    pub async fn orchestrate_task_bounded(
        &self,
        model: &ModelRef,
        mode: PermissionProfile,
        goal: &str,
        approver: Arc<dyn Approver>,
        clarifier: Arc<dyn leveler_agent::Clarifier>,
        sandbox: bool,
        observer: &mut dyn FnMut(EngineEvent),
        cancellation: tokio_util::sync::CancellationToken,
        existing_session: Option<leveler_core::SessionId>,
        max_rounds: u32,
    ) -> Result<(leveler_core::SessionId, TaskReport), AppError> {
        self.orchestrate_task_with_policy(
            model,
            mode,
            goal,
            approver,
            clarifier,
            sandbox,
            observer,
            cancellation,
            existing_session,
            leveler_agent::ContinuationPolicy::bounded(max_rounds),
            leveler_agent::StepLimits::default(),
        )
        .await
    }

    #[allow(clippy::too_many_arguments)]
    async fn orchestrate_task_with_policy(
        &self,
        model: &ModelRef,
        mode: PermissionProfile,
        goal: &str,
        approver: Arc<dyn Approver>,
        clarifier: Arc<dyn leveler_agent::Clarifier>,
        sandbox: bool,
        observer: &mut dyn FnMut(EngineEvent),
        cancellation: tokio_util::sync::CancellationToken,
        // Reuse this session (interactive turns) instead of creating a fresh one
        // per turn — otherwise every orchestrated turn spawns an orphan session
        // and the current session never records the work. `None` = one-shot.
        existing_session: Option<leveler_core::SessionId>,
        continuation: leveler_agent::ContinuationPolicy,
        limits: leveler_agent::StepLimits,
    ) -> Result<(leveler_core::SessionId, TaskReport), AppError> {
        let engine = self
            .engine_for(model, mode, sandbox, approver, clarifier)
            .await?;
        let spec = TaskSpec {
            repository: self.layout.repo_root.clone(),
            goal: goal.to_string(),
            mode,
            sandbox,
            kind: ExecutionKind::Orchestrate,
            continuation,
            limits,
            verification: verification_plan_for_root(&self.layout.repo_root),
        };
        let db = self.open_database().await?;
        let repo = SessionRepository::new(&db);
        let session_id = match existing_session {
            Some(id) => {
                repo.set_execution(
                    &id,
                    mode_str(mode),
                    sandbox,
                    ExecutionKind::Orchestrate.as_str(),
                    leveler_core::now(),
                )
                .await?;
                id
            }
            None => engine
                .create_task(&spec)
                .await
                .map_err(crate::session::app_error_from_engine)?,
        };
        repo.update_status(
            &session_id,
            SessionStatus::Running,
            AgentState::Understand,
            leveler_core::now(),
        )
        .await?;

        let result = engine
            .run(&session_id, &spec, observer, cancellation)
            .await
            .map_err(crate::session::app_error_from_engine);

        // `status` is the operational position; the verified-vs-unverified
        // distinction lives in the `outcome` column (stamped by the engine).
        let (status, state) = match &result {
            Ok(report) => match report.outcome {
                TaskOutcome::Verified => (SessionStatus::Completed, AgentState::Complete),
                TaskOutcome::CompletedUnverified => {
                    (SessionStatus::Completed, AgentState::Complete)
                }
                TaskOutcome::BudgetLimited => (SessionStatus::Incomplete, AgentState::Execute),
                TaskOutcome::Interrupted => (SessionStatus::Interrupted, AgentState::Execute),
                TaskOutcome::Failed => (SessionStatus::Failed, AgentState::Failed),
            },
            Err(AppError::Agent(leveler_agent::AgentError::Cancelled)) => {
                (SessionStatus::Interrupted, AgentState::Execute)
            }
            Err(_) => (SessionStatus::Failed, AgentState::Failed),
        };
        repo.update_status(&session_id, status, state, leveler_core::now())
            .await?;

        result.map(|report| (session_id, report))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn typescript_repo_discovers_node_checks() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("tsconfig.json"), "{}").unwrap();
        std::fs::write(
            dir.path().join("package.json"),
            r#"{"scripts":{"test":"vitest run"}}"#,
        )
        .unwrap();
        let plan = verification_plan_for_root(dir.path());
        assert!(plan.has_gates());
        assert!(plan.commands.iter().any(|c| c.name == "test"));
        assert!(plan.commands.iter().any(|c| c.name == "tsc"));
    }

    #[test]
    fn monorepo_discovers_a_nested_rust_workspace() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("package.json"),
            r#"{"scripts":{"format":"prettier --check ."}}"#,
        )
        .unwrap();
        let rust = dir.path().join("nested-crate");
        std::fs::create_dir_all(&rust).unwrap();
        std::fs::write(rust.join("Cargo.toml"), "[workspace]\nmembers = []\n").unwrap();

        let plan = verification_plan_for_root(dir.path());

        assert!(plan.has_gates());
        let check = plan
            .commands
            .iter()
            .find(|command| command.name == "cargo check (nested-crate)")
            .expect("nested Rust workspace should have a cargo check gate");
        assert_eq!(
            check.args,
            [
                "check",
                "--manifest-path",
                "nested-crate/Cargo.toml",
                "--workspace",
                "--quiet"
            ]
        );
        assert!(check.gating);
        assert!(
            plan.commands
                .iter()
                .any(|command| command.name == "cargo test (nested-crate)" && command.gating)
        );
    }

    #[test]
    fn explicit_config_still_wins() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("tsconfig.json"), "{}").unwrap();
        std::fs::create_dir_all(dir.path().join(".leveler")).unwrap();
        std::fs::write(
            dir.path().join(".leveler/config.yaml"),
            "verify:\n  test:\n    program: just\n    args: [check]\n",
        )
        .unwrap();
        let plan = verification_plan_for_root(dir.path());
        assert_eq!(plan.commands.len(), 1);
        assert_eq!(plan.commands[0].program, "just");
    }
}
