use std::sync::Arc;

use tokio_util::sync::CancellationToken;

use leveler_core::{ApprovalId, ClarificationId};
use leveler_execution::{ApprovalDecision, ApprovalRequest, RiskLevel};
use leveler_model::ToolCall;

use leveler_lifecycle::ProgressLedger;

use super::{
    AgentError, AgentEvent, ClarificationRequest, Executor,
    StepLimits, StopReason, SubAgentProgressSink,
};
use crate::authorization::action_fingerprint;
use crate::sub_agent::AgentRole;

impl Executor {

    /// Answer a `request_user_input` / `ask_user` tool call via the clarifier.
    pub(crate) async fn handle_ask_user(
        &self,
        call: &ToolCall,
        cancellation: &CancellationToken,
    ) -> Result<String, AgentError> {
        if cancellation.is_cancelled() {
            return Err(AgentError::Cancelled);
        }
        let question = call
            .arguments
            .get("question")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let options = call
            .arguments
            .get("options")
            .and_then(|v| v.as_array())
            .map(|a| {
                a.iter()
                    .filter_map(|x| x.as_str().map(String::from))
                    .collect()
            })
            .unwrap_or_default();
        let request = ClarificationRequest {
            id: ClarificationId::generate(),
            turn_id: None,
            tool: call.name.clone(),
            call_id: call.id.to_string(),
            action_fingerprint: action_fingerprint(call),
            question,
            options,
        };
        let answer = tokio::select! {
            biased;
            _ = cancellation.cancelled() => return Err(AgentError::Cancelled),
            answer = self.clarifier.clarify(&request) => answer,
        };
        Ok(if answer.trim().is_empty() {
            "The user did not provide an answer; proceed using your best judgment.".to_string()
        } else {
            answer
        })
    }

    /// Handle a `request_permissions` call: ask the user to approve elevated
    /// network and/or filesystem access. Returns `(granted, message, grants)`.
    pub(crate) async fn handle_request_permissions(
        &self,
        call: &ToolCall,
        cancellation: &CancellationToken,
    ) -> Result<(bool, String, crate::injected_tools::TurnPermissionGrants), AgentError> {
        use crate::injected_tools::{
            parse_permission_request, permission_grant_message, permission_request_description,
        };
        if cancellation.is_cancelled() {
            return Err(AgentError::Cancelled);
        }
        let (action, reason, grants) = parse_permission_request(&call.arguments);
        if grants.is_empty() {
            return Ok((
                false,
                "未请求任何可识别权限:请设置 network、filesystem=unrestricted 或 full_access。"
                    .to_string(),
                grants,
            ));
        }
        let description = permission_request_description(&action, &reason, grants);
        // Risk: filesystem elevation is at least as sensitive as network.
        let risk = if grants.unrestricted_fs {
            RiskLevel::Privileged
        } else {
            RiskLevel::Network
        };
        // A permission request is a risky ACTION, not a question — it is exactly
        // the yes/no an Approver exists to answer. Routing it to the Clarifier
        // instead put it outside the approval policy, so `--auto-approve` (whose
        // whole purpose is unattended driving) still stopped dead on a human.
        let request = ApprovalRequest {
            id: ApprovalId::generate(),
            turn_id: None,
            call_id: call.id.to_string(),
            action_fingerprint: action_fingerprint(call),
            tool: call.name.clone(),
            risk,
            description,
            command: None,
            paths: Vec::new(),
        };
        let decision = tokio::select! {
            biased;
            _ = cancellation.cancelled() => return Err(AgentError::Cancelled),
            decision = self.approver.decide(&request) => decision,
        };
        let granted = matches!(
            decision,
            ApprovalDecision::ApproveOnce
                | ApprovalDecision::ApproveSession
                | ApprovalDecision::ApproveAlways
        );
        let message = permission_grant_message(granted, grants);
        Ok((granted, message, grants))
    }

    /// Run one spawned sub-agent to completion on its own fresh conversation,
    /// returning text + ok + spend for parent rollup. A permit from the shared
    /// semaphore bounds how many run at once. The child streams silently —
    /// only its start/finish bubbles to the parent observer (see the batch in
    /// `drive`).
    pub(crate) async fn run_one_sub_agent(
        &self,
        id: String,
        role: AgentRole,
        files: Vec<String>,
        task: String,
        permit: Arc<tokio::sync::Semaphore>,
        progress: tokio::sync::mpsc::UnboundedSender<AgentEvent>,
        mut residual_limits: StepLimits,
        cancellation: CancellationToken,
        parent_wall: ParentWallBudget,
    ) -> SubAgentRunResult {
        let _slot = match permit.acquire().await {
            Ok(slot) => slot,
            Err(_) => {
                return SubAgentRunResult {
                    text: "sub-agent failed: concurrency slot closed".to_string(),
                    ok: false,
                    progress: ProgressLedger::default(),
                    modified_files: Vec::new(),
                };
            }
        };
        // Refresh wall residual after queue wait. A child that waited behind
        // others would otherwise keep a pre-queue residual past the parent
        // deadline and continue running after the parent budget is exhausted.
        if let Some(parent_max) = parent_wall.cap {
            let elapsed = parent_wall
                .epoch_duration_at_start
                .saturating_add(parent_wall.run_started.elapsed());
            let residual = parent_max.saturating_sub(elapsed);
            let sub_cap = crate::sub_agent::SUB_AGENT_MAX_DURATION;
            residual_limits.max_duration = Some(sub_cap.min(residual));
        }
        let _ = progress.send(AgentEvent::SubAgentProgress {
            id: id.clone(),
            active: true,
            input_tokens: 0,
            output_tokens: 0,
            cached_input_tokens: 0,
        });
        let mut child = self.child_for_role(role, files);
        // Task-level residual budgets: child cannot spend more than its share
        // of the parent remainder (Some(0) hard-blocks that dimension).
        child.step_limits = residual_limits;
        // Capture ProgressUpdated even when the child is cancelled mid-run so
        // partial spend still rolls up to the parent.
        let partial = std::sync::Arc::new(std::sync::Mutex::new(ProgressLedger::default()));
        let partial_obs = partial.clone();
        let mut capture = move |event: AgentEvent| {
            if let AgentEvent::ProgressUpdated { ledger } = event
                && let Ok(mut guard) = partial_obs.lock()
            {
                *guard = ledger;
            }
        };
        let mut sink = SubAgentProgressSink::new(id, progress);
        // Box the recursive future (agent → spawn_agent → agent) so its size is
        // finite.
        let run = child.run(&task, &mut capture, &mut sink, cancellation);
        match Box::pin(run).await {
            Ok(outcome) => {
                let ok = matches!(
                    outcome.stop_reason,
                    StopReason::Completed
                        | StopReason::Answered
                        | StopReason::CompletedUnverified
                        | StopReason::CloseoutForced
                );
                SubAgentRunResult {
                    text: outcome.final_text,
                    ok,
                    progress: outcome.progress,
                    modified_files: outcome.modified_files,
                }
            }
            Err(AgentError::Cancelled) => {
                // Partial ProgressUpdated still rolls up so cancel cannot erase
                // commands/files/tokens already spent by the child.
                let ledger = partial.lock().map(|g| g.clone()).unwrap_or_default();
                let paths = ledger.cumulative_modified_paths.clone();
                SubAgentRunResult {
                    text: "sub-agent cancelled".to_string(),
                    ok: false,
                    progress: ledger,
                    modified_files: paths,
                }
            }
            Err(e) => {
                let ledger = partial.lock().map(|g| g.clone()).unwrap_or_default();
                let paths = ledger.cumulative_modified_paths.clone();
                SubAgentRunResult {
                    text: format!("sub-agent failed: {e}"),
                    ok: false,
                    progress: ledger,
                    modified_files: paths,
                }
            }
        }
    }
}

/// Spend + text returned from one sub-agent so the parent can roll up budgets.
#[derive(Debug, Clone)]
pub(crate) struct SubAgentRunResult {
    pub text: String,
    pub ok: bool,
    pub progress: ProgressLedger,
    pub modified_files: Vec<String>,
}

/// Parent wall-clock budget context for refreshing a child's residual duration
/// after it finishes waiting on the concurrency semaphore.
#[derive(Debug, Clone, Copy)]
pub(crate) struct ParentWallBudget {
    pub cap: Option<std::time::Duration>,
    pub epoch_duration_at_start: std::time::Duration,
    pub run_started: std::time::Instant,
}
