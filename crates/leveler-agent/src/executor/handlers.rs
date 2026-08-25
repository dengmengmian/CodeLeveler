use std::sync::Arc;

use tokio_util::sync::CancellationToken;

use leveler_core::{ApprovalId, ClarificationId};
use leveler_execution::{ApprovalDecision, ApprovalRequest, RiskLevel};
use leveler_model::ToolCall;

use leveler_lifecycle::ProgressLedger;

use super::{
    AgentError, AgentEvent, ClarificationRequest, Executor, StepLimits, StopReason,
    SubAgentProgressSink,
};
use crate::authorization::action_fingerprint;
use crate::sub_agent::{AgentRole, ChildResult};

/// Plain words for why a run ended, for a parent model rather than a log.
fn stop_reason_wording(reason: StopReason) -> String {
    match reason {
        StopReason::BudgetExhausted => "its token or cost budget ran out",
        StopReason::TurnLimitReached => "it hit the round ceiling",
        StopReason::Blocked => "it declared the task blocked",
        StopReason::PolicyBlocked => "harness policy refused its actions",
        StopReason::Stalled => "it went quiet without resolving the task",
        StopReason::Incomplete => "it stopped without finishing",
        StopReason::Completed
        | StopReason::Answered
        | StopReason::CompletedUnverified
        | StopReason::CloseoutForced => "",
    }
    .to_string()
}

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
        let outcome = tokio::select! {
            biased;
            _ = cancellation.cancelled() => return Err(AgentError::Cancelled),
            outcome = self.clarifier.clarify(&request) => outcome,
        };
        // Only `Answered` speaks for the user. Everything else must be
        // reported as the absence of a user, so the model cannot mistake an
        // unattended run or a timeout for "the user said nothing is needed".
        Ok(match outcome {
            crate::executor::ClarifyOutcome::Answered(text) if !text.trim().is_empty() => text,
            crate::executor::ClarifyOutcome::Answered(_) | crate::executor::ClarifyOutcome::Skipped => {
                "The user saw this question and chose to skip it; proceed using your best judgment."
                    .to_string()
            }
            crate::executor::ClarifyOutcome::Unattended => {
                "No user is available to answer in this unattended run — this is NOT a user reply. \
                 Continue only if the task can be completed without this information; otherwise \
                 finish what is safely possible and state explicitly what is missing instead of guessing."
                    .to_string()
            }
            crate::executor::ClarifyOutcome::TimedOut => {
                "The user did not respond in time — this is NOT a user reply. Continue only if the \
                 task can proceed without the answer; otherwise state explicitly what is missing."
                    .to_string()
            }
            crate::executor::ClarifyOutcome::Cancelled => return Err(AgentError::Cancelled),
        })
    }

    /// Handle a `request_permissions` call: ask the user to approve elevated
    /// network and/or filesystem access.
    pub(crate) async fn handle_request_permissions(
        &self,
        call: &ToolCall,
        cancellation: &CancellationToken,
    ) -> Result<crate::injected_tools::PermissionRequestOutcome, AgentError> {
        use crate::injected_tools::{
            PermissionRequestOutcome, parse_permission_request, permission_denied_by_user_message,
            permission_denied_unattended_message, permission_grant_message,
            permission_request_description,
        };
        if cancellation.is_cancelled() {
            return Err(AgentError::Cancelled);
        }
        let (action, reason, grants) = parse_permission_request(&call.arguments);
        if grants.is_empty() {
            return Ok(PermissionRequestOutcome::Invalid {
                message:
                    "未请求任何可识别权限:请设置 network、filesystem=unrestricted 或 full_access。"
                        .to_string(),
            });
        }
        // 完全访问 means no prompts at all. This path calls the approver
        // directly, so without this check it bypasses `ApprovalPolicy::evaluate`
        // — which returns Auto for FullAccess before anything else — and
        // interrupts a user who opted out of prompting, to grant a permission
        // they already hold.
        if self.tool_context.policy.mode == leveler_execution::PermissionProfile::FullAccess {
            return Ok(PermissionRequestOutcome::Granted {
                message: permission_grant_message(true, grants),
                grants,
            });
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
            agent_id: self.agent_id.clone(),
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
        if granted {
            return Ok(PermissionRequestOutcome::Granted {
                message: permission_grant_message(true, grants),
                grants,
            });
        }
        if self.approver.has_human() {
            Ok(PermissionRequestOutcome::DeniedByUser {
                requested: grants,
                message: permission_denied_by_user_message(),
            })
        } else {
            Ok(PermissionRequestOutcome::DeniedUnattended {
                requested: grants,
                message: permission_denied_unattended_message(),
            })
        }
    }

    /// Run one independent reviewer child.
    ///
    /// R007b N7: this is the second entrance to the same child primitive the
    /// `spawn_agent` tool uses — same lifecycle, registry, limits, cancellation
    /// and event attribution. It exists because the reviewer designation was
    /// only honoured when a model chose to delegate, which R008 and R009 both
    /// declined to do. `AgentRole::parse` deliberately does not accept
    /// "reviewer", so this role can only be entered from here.
    ///
    /// The caller owns the `SubAgentStarted` / `SubAgentFinished` pair, exactly
    /// as `drive.rs` does for the model-facing entrance; the transient
    /// progress/activity events are forwarded into `observer` when the child
    /// returns.
    pub async fn run_reviewer_child(
        &self,
        id: String,
        brief: String,
        files: Vec<String>,
        observer: &mut (dyn FnMut(AgentEvent) + Send),
        cancellation: CancellationToken,
    ) -> DelegatedChildResult {
        let (progress_tx, mut progress_rx) = tokio::sync::mpsc::unbounded_channel();
        let parent_wall = ParentWallBudget {
            cap: self.step_limits.max_duration,
            epoch_duration_at_start: std::time::Duration::ZERO,
            run_started: std::time::Instant::now(),
        };
        // R013r: an unbounded reviewer burned the full 100-round turn ceiling
        // reading a repo it was only asked to judge. The bound now lives on
        // the role's capability profile; a reviewer that has not concluded by
        // then returns what it has (INCOMPLETE_PARTIAL keeps its findings)
        // instead of burning the parent's budget.
        let reviewer_rounds = crate::sub_agent::ChildProfile::resolve(AgentRole::Reviewer)
            .max_rounds()
            .unwrap_or(0);
        let result = self
            .run_one_sub_agent_on(
                id,
                AgentRole::Reviewer,
                files,
                None,
                Vec::new(),
                reviewer_rounds,
                brief,
                Arc::new(tokio::sync::Semaphore::new(1)),
                progress_tx,
                self.step_limits,
                cancellation,
                parent_wall,
            )
            .await;
        // The sender was moved into the call, so the channel is closed by now
        // and this drains what the child emitted while it ran.
        while let Ok(event) = progress_rx.try_recv() {
            observer(event);
        }
        DelegatedChildResult {
            ok: result.result.status.completed(),
            result: result.result,
            modified_files: result.modified_files,
            findings: result.findings,
        }
    }

    /// Run one spawned sub-agent to completion on its own fresh conversation,
    /// returning text + ok + spend for parent rollup. A permit from the shared
    /// semaphore bounds how many run at once. The child streams silently —
    /// only its start/finish bubbles to the parent observer (see the batch in
    /// `drive`).
    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn run_one_sub_agent_on(
        &self,
        id: String,
        role: AgentRole,
        files: Vec<String>,
        model_override: Option<leveler_model::ModelRef>,
        agent_tools: Vec<String>,
        agent_max_rounds: u32,
        task: String,
        permit: Arc<tokio::sync::Semaphore>,
        progress: tokio::sync::mpsc::UnboundedSender<AgentEvent>,
        residual_limits: StepLimits,
        cancellation: CancellationToken,
        parent_wall: ParentWallBudget,
    ) -> SubAgentRunResult {
        self.sub_agent_run_future(
            id,
            role,
            files,
            model_override,
            agent_tools,
            agent_max_rounds,
            task,
            permit,
            progress,
            residual_limits,
            cancellation,
            parent_wall,
        )
        .await
    }

    /// Build the child's run as an OWNED `'static` future, so a background
    /// delegation can be `tokio::spawn`ed and keep executing while the parent's
    /// model loop continues (V2 background-first). The child executor and every
    /// capture it needs are constructed here, synchronously, from `&self`; the
    /// returned future borrows nothing from the parent.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn sub_agent_run_future(
        &self,
        id: String,
        role: AgentRole,
        files: Vec<String>,
        model_override: Option<leveler_model::ModelRef>,
        agent_tools: Vec<String>,
        agent_max_rounds: u32,
        task: String,
        permit: Arc<tokio::sync::Semaphore>,
        progress: tokio::sync::mpsc::UnboundedSender<AgentEvent>,
        residual_limits: StepLimits,
        cancellation: CancellationToken,
        parent_wall: ParentWallBudget,
    ) -> impl std::future::Future<Output = SubAgentRunResult> + Send + 'static {
        let prepared_child = self
            .child_for_role_on(role, files, model_override)
            .with_agent_id(id.clone());
        let hook_runner = self.hook_runner.clone();
        run_prepared_sub_agent(
            prepared_child,
            hook_runner,
            id,
            role,
            agent_tools,
            agent_max_rounds,
            task,
            permit,
            progress,
            residual_limits,
            cancellation,
            parent_wall,
        )
    }
}

/// The owned body of one sub-agent run (see [`Executor::sub_agent_run_future`]).
#[allow(clippy::too_many_arguments)]
async fn run_prepared_sub_agent(
    mut child: Executor,
    hook_runner: leveler_execution::HookRunner,
    id: String,
    role: AgentRole,
    agent_tools: Vec<String>,
    agent_max_rounds: u32,
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
                result: ChildResult::new(false, "", "no concurrency slot was available to run it"),
                progress: ProgressLedger::default(),
                modified_files: Vec::new(),
                findings: Vec::new(),
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
    // Sub-agents run concurrently and silently; without an event here there
    // is no way to observe what a fleet of them is doing.
    if hook_runner.has_lifecycle() {
        hook_runner
            .run_lifecycle(
                leveler_execution::LifecycleEvent::SubagentStart,
                &format!(r#"{{"id":"{id}","role":"{}"}}"#, role.label()),
                &cancellation,
            )
            .await;
    }
    // A definition that declares its own tools / round budget binds every
    // spawn of it — otherwise the field is decoration.
    child.apply_agent_policy(&agent_tools, agent_max_rounds);
    // Task-level residual budgets: child cannot spend more than its share
    // of the parent remainder (Some(0) hard-blocks that dimension).
    child.step_limits = residual_limits;
    // Capture ProgressUpdated even when the child is cancelled mid-run so
    // partial spend still rolls up to the parent. Also re-emit tool
    // start/finish as attributed SubAgentActivity for the UI.
    let partial = std::sync::Arc::new(std::sync::Mutex::new(ProgressLedger::default()));
    let partial_obs = partial.clone();
    // R007b N1: a child stopped by its budget used to hand back only the
    // stop string, throwing away everything it had already established.
    // Keep its last report so an interrupted run is partial, not empty.
    let said = std::sync::Arc::new(std::sync::Mutex::new(String::new()));
    let said_obs = said.clone();
    // Typed findings the child has reported so far. Captured from every
    // ledger snapshot — not just a terminal one — so an interrupted child
    // still hands over what it had established (the same principle as
    // `said` above).
    let findings = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let findings_obs = findings.clone();
    let activity_id = id.clone();
    let activity_tx = progress.clone();
    let mut capture = move |event: AgentEvent| match &event {
        AgentEvent::ProgressUpdated { ledger } => {
            if let Ok(mut guard) = partial_obs.lock() {
                *guard = ledger.clone();
            }
        }
        AgentEvent::EvidenceLedgerUpdated { ledger } => {
            if let Ok(mut guard) = findings_obs.lock() {
                *guard = ledger.findings.clone();
            }
        }
        AgentEvent::AssistantText(text) if !text.trim().is_empty() => {
            if let Ok(mut guard) = said_obs.lock() {
                *guard = text.clone();
            }
        }
        AgentEvent::ToolCall {
            name, arguments, ..
        } => {
            let _ = activity_tx.send(AgentEvent::SubAgentActivity {
                id: activity_id.clone(),
                phase: "tool_started".to_string(),
                tool: name.clone(),
                preview: cap_activity_preview(arguments),
                is_error: false,
            });
        }
        AgentEvent::ToolResult {
            name,
            is_error,
            preview,
            ..
        } => {
            let _ = activity_tx.send(AgentEvent::SubAgentActivity {
                id: activity_id.clone(),
                phase: "tool_finished".to_string(),
                tool: name.clone(),
                preview: cap_activity_preview(preview),
                is_error: *is_error,
            });
        }
        // An ownership transition decides what this child may mutate, and
        // child activity is transient — forward it verbatim so it lands on
        // the parent's DURABLE delegation channel. Without this an offline
        // audit cannot distinguish an authorized write from a bypass (the M7
        // measurement failure).
        AgentEvent::DelegationStage { action, .. } if action.starts_with("ownership_") => {
            let _ = activity_tx.send(event.clone());
        }
        _ => {}
    };
    let mut sink = SubAgentProgressSink::new(id, progress);
    // Box the recursive future (agent → spawn_agent → agent) so its size is
    // finite.
    let hook_token = cancellation.clone();
    let run = child.run(&task, &mut capture, &mut sink, cancellation);
    let outcome = Box::pin(run).await;
    if hook_runner.has_lifecycle() {
        let ok = outcome.is_ok();
        hook_runner
            .run_lifecycle(
                leveler_execution::LifecycleEvent::SubagentStop,
                &format!(r#"{{"ok":{ok}}}"#),
                &hook_token,
            )
            .await;
    }
    let said_before_stopping = || said.lock().map(|g| g.clone()).unwrap_or_default();
    let reported_findings = findings.lock().map(|g| g.clone()).unwrap_or_default();
    match outcome {
        Ok(outcome) => {
            let completed = matches!(
                outcome.stop_reason,
                StopReason::Completed
                    | StopReason::Answered
                    | StopReason::CompletedUnverified
                    | StopReason::CloseoutForced
            );
            // A non-clean stop's `final_text` is usually the SYNTHETIC stop
            // sentence ("reached the N-round ceiling…"), not the child's
            // findings — R013r lost a voiced finding to exactly that. For
            // interrupted runs, prefer what the child actually said; the
            // stop reason is carried separately.
            let mut findings = if completed {
                outcome.final_text
            } else {
                let said = said_before_stopping();
                if said.trim().is_empty() {
                    outcome.final_text
                } else {
                    said
                }
            };
            // Typed findings ARE a result. An empty prose wrap-up must
            // not be classified COMPLETED_NO_FINDINGS when the child
            // already recorded structured findings.
            if findings.trim().is_empty() && !reported_findings.is_empty() {
                findings = format!(
                    "{} structured finding(s) reported.",
                    reported_findings.len()
                );
            }
            let stop_reason = if completed {
                String::new()
            } else {
                stop_reason_wording(outcome.stop_reason)
            };
            SubAgentRunResult {
                result: ChildResult::new(completed, &findings, stop_reason),
                progress: outcome.progress,
                modified_files: outcome.modified_files,
                findings: reported_findings,
            }
        }
        Err(AgentError::Cancelled) => {
            // Partial ProgressUpdated still rolls up so cancel cannot erase
            // commands/files/tokens already spent by the child.
            let ledger = partial.lock().map(|g| g.clone()).unwrap_or_default();
            let paths = ledger.cumulative_modified_paths.clone();
            SubAgentRunResult {
                result: ChildResult::new(
                    false,
                    &said_before_stopping(),
                    "stopped before it could finish",
                ),
                progress: ledger,
                modified_files: paths,
                findings: reported_findings,
            }
        }
        Err(e) => {
            let ledger = partial.lock().map(|g| g.clone()).unwrap_or_default();
            let paths = ledger.cumulative_modified_paths.clone();
            SubAgentRunResult {
                result: ChildResult::new(false, &said_before_stopping(), e.to_string()),
                progress: ledger,
                modified_files: paths,
                findings: reported_findings,
            }
        }
    }
}

/// Outcome of a child the **harness** launched, rather than one a model asked
/// for with `spawn_agent`.
#[derive(Debug, Clone)]
pub struct DelegatedChildResult {
    /// Whether the child reached a clean terminal state.
    pub ok: bool,
    /// What the child established, and how its run ended.
    pub result: ChildResult,
    /// Files the child touched (a reviewer is read-only, so normally empty).
    pub modified_files: Vec<String>,
    /// Typed findings the child reported (partial ones survive an abnormal
    /// stop). Still keyed by the CHILD's ids — the consumer adopts them.
    pub findings: Vec<leveler_lifecycle::FindingRecord>,
}

/// Spend + structured result from one sub-agent so the parent can roll up
/// budgets and read what actually happened.
#[derive(Debug, Clone)]
pub(crate) struct SubAgentRunResult {
    pub result: ChildResult,
    pub progress: ProgressLedger,
    pub modified_files: Vec<String>,
    /// Typed findings captured from the child's ledger snapshots.
    pub findings: Vec<leveler_lifecycle::FindingRecord>,
}

/// Cap UI previews so concurrent sub-agent activity cannot flood the event bus.
fn cap_activity_preview(s: &str) -> String {
    const MAX: usize = 160;
    leveler_core::truncate_head_bytes(s.trim(), MAX, "…")
}

/// Parent wall-clock budget context for refreshing a child's residual duration
/// after it finishes waiting on the concurrency semaphore.
#[derive(Debug, Clone, Copy)]
pub(crate) struct ParentWallBudget {
    pub cap: Option<std::time::Duration>,
    pub epoch_duration_at_start: std::time::Duration,
    pub run_started: std::time::Instant,
}
