//! The ToolHost boundary: the ONE path by which a model-proposed tool call
//! becomes an execution (convergence plan phase 2).
//!
//! Pipeline: side-effect barrier → pre-hooks → permission rules → profile
//! policy → auto-review/approval → barrier again (the approval outcome must
//! be durable before the side effect it authorizes) → execution. Admission
//! returns an [`AdmittedCall`], the only value [`Executor::dispatch`] and
//! [`Executor::dispatch_raw`] accept — execution without admission does not
//! typecheck, and `tests/tool_host_boundary.rs` trips if any other file in
//! this crate reaches `registry.execute` or the hook gate directly.
//!
//! The loop (drive.rs) keeps what the plan assigns to it: batch scheduling,
//! concurrency constraints, and result feedback order. It cannot execute.

use std::collections::HashSet;

use tokio_util::sync::CancellationToken;

use leveler_core::ApprovalId;
use leveler_execution::{
    ApprovalDecision, ApprovalOutcome, ApprovalRequest, AuthorizationEvidence, CommandView,
    PendingApproval, PolicyDenial, PolicyResolution, Requirement, ResolvedExecutionPolicy,
    ReviewVerdict, RiskLevel, WriteScope, command_is_destructive,
};
use leveler_lifecycle::PlanStep;
use leveler_model::{ContentPart, ToolCall};
use leveler_tools::{ToolContext, ToolError, ToolRegistry};

use super::dispatch::{
    collect_modified, extract_applied_diff, extract_executed_commands, extract_image, extract_plan,
};
use super::{AgentError, Executor};
use crate::authorization::{
    action_fingerprint, approval_signature, call_needs_host_escape, command_line_for_match,
    extract_command,
};

/// A tool call that has passed the full admission pipeline. Constructed only
/// by [`Executor::admit`]; possession is the proof that hooks, rules, policy,
/// approval, and the side-effect barrier all ran for exactly this call.
pub(crate) struct AdmittedCall {
    pub(crate) call: ToolCall,
    /// The execution context, frozen to `resolved` (PR 5): the tool reads its
    /// write scope and network policy from here, never from the live profile.
    /// Private: only the host dereferences it.
    ctx: ToolContext,
    /// The immutable policy this call executes under.
    resolved: ResolvedExecutionPolicy,
}

impl AdmittedCall {
    /// Hand the call back for the loop's post-execution bookkeeping.
    pub(crate) fn into_call(self) -> ToolCall {
        self.call
    }

    /// The policy this call was admitted under. Immutable: a profile switch
    /// or a grant made after admission reaches the next call, not this one.
    pub(crate) fn resolved(&self) -> &ResolvedExecutionPolicy {
        &self.resolved
    }

    /// The context the tool executes with — already frozen to `resolved`.
    pub(crate) fn execution_context(&self) -> &ToolContext {
        &self.ctx
    }
}

/// What asking the reviewer / human produced for a [`PendingApproval`].
pub(crate) enum AskOutcome {
    Allowed(AuthorizationEvidence),
    DeniedByUser,
    /// Nobody was asked: a headless approver refused, or the reviewer did.
    DeniedUnattended(String),
    Cancelled,
    /// The permission profile changed while the question waited, so the
    /// question is void. Nobody granted or refused anything: the caller must
    /// re-resolve the call under the profile now in force (Full => Allow).
    Superseded,
}

/// How many times one call may be re-resolved because the permission profile
/// changed while it was waiting for an answer. This is a mechanical guard
/// against a flapping profile, not a policy: the profile owner drives every
/// iteration, and reaching this bound is reported as a refusal, never spun on.
pub(crate) const MAX_SUPERSEDE_RETRIES: usize = 8;

/// A call whose admission ALREADY happened and is on the durable record — a
/// `ToolCallStarted` the engine found dangling after a crash.
///
/// Reconciliation re-runs such a call without asking again: the human already
/// decided, and the decision is in the event log. What it must NOT do is
/// re-run something whose replay could act on the world, so the only way to
/// build one is [`PriorlyAdmitted::from_persisted`], which refuses anything
/// the tool has not declared replay-safe.
pub struct PriorlyAdmitted {
    name: String,
    arguments: serde_json::Value,
}

impl PriorlyAdmitted {
    /// Rebuild an admitted call from what the event log recorded. `None` when
    /// the tool is unknown to this build or never declared replay safety —
    /// the caller must then stop for human reconciliation instead.
    pub fn from_persisted(
        registry: &ToolRegistry,
        name: &str,
        arguments: serde_json::Value,
    ) -> Option<Self> {
        registry.replay_is_side_effect_free(name).then(|| Self {
            name: name.to_string(),
            arguments,
        })
    }
}

/// Execute a previously-admitted call during crash reconciliation.
///
/// This exists so the host has ONE place that runs a tool. Recovery used to
/// call `ToolRegistry::execute` from the engine, which meant the "single
/// ToolHost boundary" was true of the agent crate and false of the system.
/// Returns `(is_error, output)`; a failure is a result to record, never a
/// panic and never a silent success.
pub async fn reconcile(
    registry: &ToolRegistry,
    context: ToolContext,
    call: &PriorlyAdmitted,
    cancellation: &CancellationToken,
) -> (bool, String) {
    match registry
        .execute(
            &call.name,
            call.arguments.clone(),
            context,
            cancellation.child_token(),
        )
        .await
    {
        Ok(output) => (output.is_error, output.content),
        Err(error) => (true, format!("tool error: {error}")),
    }
}

/// An `ApproveAlways` decision waiting to become a durable permission rule.
/// Held until the barrier confirms the approval resolution is on disk, so a
/// crash can never leave a permanent grant that the event log does not explain.
pub(crate) enum PendingStandingGrant {
    Legacy {
        tool: String,
        command_line: Option<String>,
        paths: Vec<String>,
    },
    Resource {
        request: leveler_core::GrantRequest,
        scope: leveler_core::GrantScope,
    },
}

/// Why admission did not produce an [`AdmittedCall`].
pub(crate) enum AdmitError {
    /// The host refused the call (hook/rule/policy/approval). The loop feeds
    /// the reason back to the model as the call's errored result.
    Refused { call: ToolCall, reason: String },
    /// The host itself failed (the durability barrier could not commit). The
    /// run aborts before the tool executes — never a fake success.
    Fatal(AgentError),
}

impl Executor {
    /// Admit one tool call through the host pipeline. `parallel` marks a call
    /// the loop will defer to the read-only concurrent batch: such tools are
    /// side-effect-free by declaration, so the post-approval barrier wait is
    /// skipped (there is no side effect for a crash to lose).
    pub(crate) async fn admit(
        &self,
        call: ToolCall,
        mut ctx: ToolContext,
        parallel: bool,
        session_approved: &mut HashSet<String>,
        cancellation: &CancellationToken,
    ) -> Result<AdmittedCall, AdmitError> {
        if ctx.execution.workspace.is_none()
            && self
                .registry
                .get(&call.name)
                .is_some_and(|tool| tool.requires_workspace())
        {
            return Err(AdmitError::Refused {
                reason: "tool requires a primary workspace; no workspace is attached".into(),
                call,
            });
        }
        if !ctx.policy.unrestricted_execution()
            && self
                .capabilities
                .as_ref()
                .is_some_and(|state| !state.permits_tool(&call.name))
        {
            return Err(AdmitError::Refused {
                reason: format!("tool {} capability is not exposed", call.name),
                call,
            });
        }
        if self.registry.runs_command(&call.name) || self.registry.mutates_files(&call.name) {
            let lease = tokio::select! {
                lease = ctx.execution.command_gate.clone().lock_owned() => lease,
                _ = cancellation.cancelled() => return Err(AdmitError::Fatal(AgentError::Cancelled)),
            };
            ctx = ctx.with_command_lease(lease);
            if !ctx.policy.unrestricted_execution() {
                // Ownership may have changed while this command waited for the gate.
                ctx.policy.command_write_allowlist =
                    self.effective_write_allowlist().map(std::sync::Arc::new);
                let owner = self.agent_id.as_deref().unwrap_or("parent");
                let mut foreign = self.ownership.paths_owned_by_others(owner);
                if let Some(tasks) = &self.background_tasks {
                    let background_foreign = tasks
                        .foreign_write_paths(ctx.session_scope(), ctx.writer_scope())
                        .await;
                    foreign.extend(background_foreign);
                }
                foreign.sort();
                foreign.dedup();
                ctx = ctx.with_foreign_owned_paths(foreign);
            }
        }
        // A delegated agent's call has no canonical event of its own — the
        // parent loop announced nothing for it — so record one, attributed,
        // on the same queue the barrier drains. Without this a worker child
        // that crashes mid-edit leaves a side effect the host cannot see.
        if let (Some(barrier), Some(agent_id)) = (&self.event_barrier, &self.agent_id) {
            barrier.record_child_tool_event(super::ChildToolEvent::Started {
                agent_id: agent_id.clone(),
                call_id: call.id.as_str().to_string(),
                name: call.name.clone(),
                arguments: super::dispatch::compact_json(&call.arguments),
                // The harness owns the registry, so it stamps the risk the
                // engine records.
                risk: self.registry.get(&call.name).map(|tool| tool.risk()),
            });
        }
        // Side-effect barrier, first wait: the announcing `ToolCallStarted`
        // must be durable before ANYTHING can act on this call — the pre-tool
        // hooks below are external side effects themselves. A flush failure
        // aborts the run before the tool executes (never a fake success).
        if let Some(barrier) = &self.event_barrier
            && let Err(error) = barrier.flush().await
        {
            return Err(AdmitError::Fatal(error.into()));
        }
        let mut pending_always: Option<PendingStandingGrant> = None;
        let mut resolved = match self
            .authorize_with_cancellation(
                &call,
                &ctx,
                session_approved,
                &mut pending_always,
                cancellation,
            )
            .await
        {
            Ok(resolved) => resolved,
            Err(reason) => return Err(AdmitError::Refused { call, reason }),
        };
        // Side-effect barrier, second wait: authorization may have produced
        // ApprovalRequested/Resolved — the decision must be durable before
        // the tool it authorized can produce a side effect (else a crash
        // leaves an approved side effect that resume sees as still pending
        // approval). Parallel-batch tools skip this: read-only and
        // side-effect-free by declaration.
        if !parallel
            && let Some(barrier) = &self.event_barrier
            && let Err(error) = barrier.flush().await
        {
            return Err(AdmitError::Fatal(error.into()));
        }
        if let Some(expected) = resolved.resource_grant() {
            match self.resolve_resource_request(&call, &ctx).await {
                Ok(Some(current)) if &current == expected => {},
                _ => return Err(AdmitError::Refused { call, reason: "resource identity changed while authorization was pending; request fresh approval".into() }),
            }
        }
        // The approval outcome is durable now, so a standing "always" grant can
        // be written without the risk of outliving an unresolved approval in
        // the log. Parallel-batch calls take the same order: they are
        // side-effect-free, but the grant is still durable state.
        if let Some(grant) = pending_always {
            if parallel
                && let Some(barrier) = &self.event_barrier
                && let Err(error) = barrier.flush().await
            {
                return Err(AdmitError::Fatal(error.into()));
            }
            match grant {
                PendingStandingGrant::Legacy {
                    tool,
                    command_line,
                    paths,
                } => self.remember_always(&tool, command_line.as_deref(), &paths),
                PendingStandingGrant::Resource { request, scope } => {
                    self.resource_grants
                        .grant(
                            &request.project_identity,
                            self.grant_session_id(),
                            scope,
                            &request.bindings,
                        )
                        .await
                        .map_err(|e| AdmitError::Fatal(AgentError::Persistence(e.to_string())))?;
                }
            }
        }
        // OWNERSHIP FENCE — last gate before an AdmittedCall can exist. The
        // announcing/approval facts are durable (barriers above); now prove
        // this runtime still owns the task. A stale runtime must not start a
        // new external side effect: abort the run with a typed error, never a
        // model-visible refusal it could loop on. This is an additional gate
        // on top of the persistence barrier, not a replacement — and it does
        // not claim exactly-once (ownership may still change between this
        // check and the tool's effect; that window belongs to transfer
        // protocols, not P2C).
        if let Some(fence) = &self.execution_fence
            && let Err(reason) = fence.ensure_current().await
        {
            return Err(AdmitError::Fatal(AgentError::StaleOwnership(reason)));
        }
        // Host openers (`open`/`xdg-open`) only work outside seatbelt;
        // elevate after approval (the user already OK'd this call). This is
        // the one post-approval widening, and it lands in the frozen policy —
        // not in a flag a tool could read back.
        if call_needs_host_escape(&call) {
            if matches!(
                resolved.write,
                WriteScope::None | WriteScope::ScopedWorkspace { .. }
            ) {
                return Err(AdmitError::Refused {
                    call,
                    reason: "host opener cannot preserve the command's structural write scope"
                        .to_string(),
                });
            }
            resolved.write = WriteScope::Unrestricted;
        }
        let ctx = ctx.with_resolved_policy(resolved.clone());
        Ok(AdmittedCall {
            call,
            ctx,
            resolved,
        })
    }

    /// Decide whether a tool call may proceed: resolve the policy, then ask
    /// if resolution says so. `Ok` carries the immutable policy the call runs
    /// under; `Err(reason)` is fed back to the model as a tool error.
    pub(crate) async fn authorize_with_cancellation(
        &self,
        call: &ToolCall,
        ctx: &ToolContext,
        session_approved: &mut HashSet<String>,
        pending_always: &mut Option<PendingStandingGrant>,
        cancellation: &CancellationToken,
    ) -> Result<ResolvedExecutionPolicy, String> {
        // A permission-profile change supersedes a question that is already
        // waiting; the answer is not "yes" or "no", it is "ask the policy
        // again". Bounded so a pathological flapping profile cannot spin here:
        // the profile owner drives each supersession, and a loop it can drive
        // forever is a bug in the owner, not a question to answer.
        for _ in 0..MAX_SUPERSEDE_RETRIES {
            let pending = match self.resolve_policy(call, ctx, cancellation).await {
                PolicyResolution::Allow(resolved) => return Ok(resolved),
                PolicyResolution::Deny(PolicyDenial { reason }) => return Err(reason),
                PolicyResolution::Ask(pending) => pending,
            };
            match self
                .ask(
                    &pending,
                    Some(session_approved),
                    Some(pending_always),
                    cancellation,
                )
                .await
            {
                AskOutcome::Allowed(evidence) => return Ok(pending.allowed(evidence)),
                AskOutcome::DeniedByUser => return Err("denied by user".to_string()),
                AskOutcome::DeniedUnattended(reason) if call.name == "remember" => {
                    let _ = reason;
                    return Err(self.park_unattended_denial(call));
                }
                AskOutcome::DeniedUnattended(reason) => return Err(reason),
                AskOutcome::Cancelled => return Err("cancelled".to_string()),
                // The profile changed while the question waited. Loop and let
                // `resolve_policy` answer under the profile now in force; a
                // switch to Full resolves to Allow here.
                AskOutcome::Superseded => continue,
            }
        }
        Err(format!(
            "permission profile changed {MAX_SUPERSEDE_RETRIES} times while `{}` waited for a \
             decision; re-run the call once the profile is settled",
            call.name
        ))
    }

    /// Pure resolution (PR 5): pre hooks → permission rules → profile policy.
    /// Produces Allow / Ask / Deny and asks nobody. The write scope and
    /// network policy come from `ctx` — the inputs the loop assembled (live
    /// profile, turn grants, this call's escalation) — and are frozen here.
    pub(crate) async fn resolve_policy(
        &self,
        call: &ToolCall,
        ctx: &ToolContext,
        cancellation: &CancellationToken,
    ) -> PolicyResolution {
        // Extract command for run_command / shell_command so the policy can
        // classify it. shell_command uses a platform wrapper for classification
        // but permission rules match the raw `cmd` string.
        let (program, args) = extract_command(call);
        let command_view = program.as_ref().map(|p| CommandView {
            program: p,
            args: &args,
        });
        let command_line = command_line_for_match(call, program.as_deref(), &args);

        // Highest-priority product contract: Full bypasses CodeLeveler
        // permission rules, hooks, Git gates and sandbox boundaries entirely.
        if ctx.policy.unrestricted_execution() {
            return PolicyResolution::Allow(ResolvedExecutionPolicy::new(
                WriteScope::Unrestricted,
                leveler_execution::NetworkScope::Internet,
                AuthorizationEvidence::Policy {
                    profile: leveler_execution::PermissionProfile::FullAccess,
                },
            ));
        }

        // Git states its own mechanical effects from argv. Two things follow
        // from that ONE parse, so they can never drift apart: which repository
        // metadata capability the call runs with (the same answer whether it
        // was auto-allowed, matched a standing rule, or was just approved),
        // and what a prompt has to say about it. The permission VERDICT itself
        // stays the policy's job (`classify_command`).
        let git_effects = program.as_deref().map(|program| {
            let executed = leveler_execution::executed_commands(program, &args);
            executed.git_effects()
        });
        // A Git invocation that writes repository metadata runs with the
        // repository's `.git` unsealed. That is a mechanical REQUIREMENT of the
        // effect, not an authority question by itself — the verdict above still
        // decides whether a person is asked, and ownership-narrowed scopes
        // (`None`, `ScopedWorkspace`) are never widened.
        let write = match (&git_effects, ctx.write_scope()) {
            (Some(git), WriteScope::Workspace { root }) if git.needs_repository_write_scope() => {
                WriteScope::WorkspaceWithGit { root }
            }
            (_, scope) => scope,
        };
        if let Some(git) = git_effects.as_ref().filter(|git| git.any_git) {
            tracing::debug!(
                tool = %call.name,
                command = command_line.as_deref().unwrap_or(""),
                capabilities = ?git.capabilities(),
                unsealed_repository_metadata = git.needs_repository_write_scope(),
                "git capability resolution"
            );
        }
        let network_scope = ctx.policy.network_scope();
        let network_allowed = matches!(network_scope, leveler_execution::NetworkScope::Internet);
        // Denied only because the profile does not reach the network by
        // default (请求批准), not by the run itself: a user approval of a call
        // whose need is the network is the grant for that call.
        let network_by_profile_default =
            !network_allowed && !ctx.policy.network_explicitly_denied();
        // The profile denies the network, and this host cannot make a
        // command honour that (Windows). Such a command is asked first and
        // runs open once approved — never under a denial nobody enforces.
        let network_unenforceable = network_by_profile_default
            && !self.network_enforceable
            && self.registry.runs_command(&call.name);
        let deny = |reason: String| PolicyResolution::Deny(PolicyDenial { reason });
        let allow = |authorization: AuthorizationEvidence| {
            PolicyResolution::Allow(ResolvedExecutionPolicy::new(
                write.clone(),
                network_scope.clone(),
                authorization,
            ))
        };

        let args_json = serde_json::to_string(&call.arguments).unwrap_or_else(|_| "{}".into());
        match self
            .hook_runner
            .run_pre(&call.name, &args_json, cancellation)
            .await
        {
            leveler_execution::PreHookResult::Allow => {}
            leveler_execution::PreHookResult::Deny(reason) => return deny(reason),
        }

        let risk = self
            .registry
            .get(&call.name)
            .map(|t| t.risk())
            .unwrap_or(RiskLevel::Safe);
        let approval_reason = if let Some(tool) = self.registry.get(&call.name) {
            tokio::select! {
                biased;
                _ = cancellation.cancelled() => return deny("cancelled".into()),
                reason = tool.admission_reason(&call.arguments, ctx) => reason,
            }
        } else {
            None
        };
        let mut approval_reason = approval_reason
            .or_else(|| self.role_permission_reason(call))
            .or_else(|| self.refuse_unboundable_delegated_tool(call))
            .or_else(|| self.refuse_unscoped_mutation(call))
            .or_else(|| {
                let foreign = &ctx.policy.command_foreign_paths;
                (self.registry.mutates_files(&call.name)
                    && (foreign.iter().any(|path| path == "." || path.is_empty())
                        || crate::sub_agent::scopes_overlap(
                            &crate::authorization::mutation_targets(call),
                            foreign,
                        )))
                .then(|| "修改其他执行者当前拥有的路径，需要批准本次操作".into())
            });

        // An MCP server is a separate process CodeLeveler launches with no OS
        // sandbox, so a network denial cannot be applied to it. Refuse rather
        // than run it under an authority this runtime cannot enforce — the
        // same reasoning that keeps MCP away from a delegated agent, whose
        // claimed write scope it also could not honour.
        if ctx.policy.network_explicitly_denied() && call.name.starts_with("mcp__") {
            return deny(format!(
                "{} is unavailable while network access is denied for this run: an \
                 MCP server is a separate process outside the sandbox, so the \
                 denial cannot be enforced on it.",
                call.name
            ));
        }
        // The read-only overlay (`leveler plan` / plan collaboration) admits
        // Safe tools only, whatever the profile would otherwise allow. It is
        // orthogonal to the three-tier profile, so it is its own gate.
        if ctx.policy.read_only
            && risk != RiskLevel::Safe
            && ctx.policy.mode() == leveler_execution::PermissionProfile::Assisted
        {
            approval_reason.get_or_insert_with(|| {
                "read-only turn requires approval for this exact action".into()
            });
        } else if ctx.policy.read_only && risk != RiskLevel::Safe {
            return deny(format!(
                "tool `{}` is not permitted in a read-only (plan) turn (risk {risk:?}): \
                 only observation tools run here",
                call.name
            ));
        }
        // A confined profile FORBIDS a Privileged/Destructive tool rather than
        // offering it for approval. Deliberately on the tool's DECLARED risk,
        // before the per-command bump below: a destructive shell command is
        // still a prompt (the user may well want that `rm`), while a tool that
        // is destructive by nature is not available at all under a confined
        // profile.
        //
        // Both of these gates used to live in `ToolRegistry::execute`, which
        // made the registry a third authorization owner alongside the host and
        // the rules. Same answers, one owner.
        let profile = ctx.policy.mode();
        if !profile.permits(risk) {
            return deny(format!(
                "tool `{}` is not permitted in {profile:?} mode (risk {risk:?})",
                call.name
            ));
        }

        let self_consent = program
            .as_deref()
            .is_some_and(|program| leveler_execution::is_self_consent_command(program, &args));
        if self_consent {
            if profile != leveler_execution::PermissionProfile::Assisted {
                return deny("CodeLeveler's consent commands require direct human action under Restricted authority".into());
            }
            approval_reason.get_or_insert_with(|| "This exact command invokes CodeLeveler's human consent surface; a human must approve it".into());
        }

        // An agent or skill definition changes what future sessions run, so a
        // human confirms each exact proposal under Auto and Restricted. Full
        // bypasses this consent gate with every other permission gate, at the
        // top of this function — a definition write reaches here only under a
        // profile that asks. The proposal is validated first: a contradiction
        // is refused to the model, never put to the user as a question.
        let definition_write = if crate::agent_registry::is_agent_definition_write(&call.name) {
            Some(crate::agent_registry::authoring_preflight(
                &call.name,
                &call.arguments,
                ctx,
            ))
        } else if leveler_tools::tools::is_skill_definition_write(&call.name) {
            Some(leveler_tools::tools::skill_authoring_preflight(
                &call.name,
                &call.arguments,
                ctx,
            ))
        } else {
            None
        };
        if let Some(proposal) = definition_write {
            let description = match proposal {
                Ok(description) => description,
                Err(reason) => return deny(reason),
            };
            let signature = action_fingerprint(call);
            return PolicyResolution::Ask(Box::new(PendingApproval {
                request: ApprovalRequest {
                    id: ApprovalId::generate(),
                    turn_id: None,
                    call_id: call.id.to_string(),
                    agent_id: self.agent_id.clone(),
                    action_fingerprint: signature.clone(),
                    tool: call.name.clone(),
                    risk,
                    description,
                    command: None,
                    paths: Vec::new(),
                    grant: None,
                },
                signature,
                write,
                network_scope: network_scope.clone(),
                command_line: None,
                scoped_paths: Vec::new(),
            }));
        }

        // A tool's declared risk is static: `run_command` carries the same level
        // whether it runs `ls` or `rm -rf`. Name the deletion in the prompt, so
        // it does not read as harmless as a listing.
        let risk = match command_view.as_ref().map(command_is_destructive) {
            Some(true) if risk < RiskLevel::Destructive => RiskLevel::Destructive,
            _ => risk,
        };

        // Paths the call touches, for `write_path_glob` rules, the approval prompt,
        // and deriving `ApproveAlways` path rules.
        let mut scoped_paths: Vec<String> = Vec::new();
        crate::authorization::collect_scoped_paths_from_call(call, &mut scoped_paths);
        let rule_paths: Vec<std::path::PathBuf> =
            scoped_paths.iter().map(std::path::PathBuf::from).collect();

        // A poisoned lock only means a rules writer panicked mid-update; the
        // rule set is append-only, so read the latest state instead of turning
        // every future dispatch into a panic.
        let rule_decision = self
            .permission_rules
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .evaluate(&call.name, command_line.as_deref(), &rule_paths);
        match rule_decision {
            leveler_execution::RuleDecision::Deny => {
                return deny("forbidden by permission rule".to_string());
            }
            // A standing rule is the user's consent; where the network cannot
            // be denied it is also consent to run the command open.
            leveler_execution::RuleDecision::Allow if !self_consent => {
                return PolicyResolution::Allow(ResolvedExecutionPolicy::new(
                    write,
                    network_scope.clone(),
                    AuthorizationEvidence::Rule,
                ));
            }
            leveler_execution::RuleDecision::Allow
            | leveler_execution::RuleDecision::Ask
            | leveler_execution::RuleDecision::NoMatch => {}
        }

        // A destination hint can reduce the prompt only when the execution
        // path also enforces this scope. Opaque clients remain Internet calls.
        let network_resource = if risk == RiskLevel::Network
            && !matches!(network_scope, leveler_execution::NetworkScope::None)
        {
            if let Some(tool) = self.registry.get(&call.name) {
                tokio::select! {
                    biased;
                    _ = cancellation.cancelled() => return deny("cancelled".into()),
                    resource = tokio::time::timeout(
                        std::time::Duration::from_secs(5),
                        tool.network_resource(&call.arguments),
                    ) => match resource {
                        Ok(Ok(resource)) => resource,
                        Ok(Err(reason)) => {
                            tracing::debug!(tool = %call.name, %reason, "network resource unresolved; broader access requires consent");
                            None
                        }
                        Err(error) => {
                            tracing::debug!(tool = %call.name, %error, "network resource resolution timed out; broader access requires consent");
                            None
                        }
                    },
                }
            } else {
                None
            }
        } else {
            None
        };
        let scoped_network_call = network_resource
            .as_ref()
            .is_some_and(|resource| network_scope.validate_resource(resource).is_ok());
        let needs_internet =
            !scoped_network_call && (risk == RiskLevel::Network || call.name.starts_with("mcp__"));
        tracing::debug!(
            tool = %call.name,
            network_scope = ?network_scope,
            network_resource = ?network_resource,
            "network destination resolution"
        );
        let requirement =
            match self
                .approval_policy
                .evaluate(profile, &call.name, risk, command_view)
            {
                Requirement::Auto if network_unenforceable || approval_reason.is_some() => {
                    Requirement::NeedApproval
                }
                // Only a known, scoped HTTP destination can avoid the Network prompt.
                Requirement::NeedApproval
                    if scoped_network_call
                        && approval_reason.is_none()
                        && risk == RiskLevel::Network
                        && matches!(rule_decision, leveler_execution::RuleDecision::NoMatch) =>
                {
                    Requirement::Auto
                }
                requirement => requirement,
            };
        // Phase 4: using a stored credential is not an ordinary auto-run even
        // when the command's mechanical effect classifies as read-only. A
        // command whose host-resolved target needs a credential escalates to a
        // real decision; an existing Session/Project grant for the exact
        // destination still short-circuits that decision below. Full never
        // reaches here as Auto-with-interception: it bypasses admission first.
        let requirement = if requirement == Requirement::Auto
            && profile != leveler_execution::PermissionProfile::FullAccess
            && command_view.is_some()
        {
            match self.resolve_resource_request(call, ctx).await {
                Ok(Some(request))
                    if request.bindings.iter().any(|binding| {
                        binding.capability == leveler_core::Capability::CredentialUse
                    }) =>
                {
                    Requirement::NeedApproval
                }
                Ok(_) => requirement,
                Err(reason) => {
                    tracing::debug!(
                        tool = %call.name,
                        %reason,
                        "credential requirement unresolved; keeping the profile decision"
                    );
                    requirement
                }
            }
        } else {
            requirement
        };
        match requirement {
            Requirement::Auto => allow(AuthorizationEvidence::Policy { profile }),
            Requirement::Forbidden => deny("forbidden by policy".to_string()),
            Requirement::NeedApproval => {
                let grant = match self.resolve_resource_request(call, ctx).await {
                    Ok(request) => request,
                    Err(reason) => {
                        tracing::debug!(tool = %call.name, %reason, "resource identity unavailable; reusable authorization disabled");
                        None
                    }
                };
                if let Some(request) = &grant {
                    match self
                        .resource_grants
                        .covers(
                            &request.project_identity,
                            self.grant_session_id(),
                            &request.bindings,
                        )
                        .await
                    {
                        Ok(true) => {
                            return allow(AuthorizationEvidence::ResourceGrant {
                                request: request.clone(),
                                scope: leveler_core::GrantScope::Once,
                            });
                        }
                        Ok(false) => {}
                        Err(error) => {
                            tracing::warn!(%error, "resource grant store unavailable; requiring fresh consent")
                        }
                    }
                }
                // Only say something the tool name and command do not already
                // say. "<tool> requested by the model" is filler, and filler in
                // a decision prompt trains people to stop reading it.
                let mut notes = Vec::new();
                if let Some(reason) = &approval_reason {
                    notes.push(reason.clone());
                }
                if let Some(git) = git_effects.as_ref().filter(|git| git.any_git) {
                    notes.push(crate::authorization::git_capability_note(git));
                }
                if call_needs_host_escape(call) {
                    notes.push(format!("{} 会打开工作区之外的应用或文件", call.name));
                }
                if network_unenforceable {
                    notes.push("此平台无法断网：批准后该命令可以联网运行".to_string());
                }
                let description = notes.join("；");
                // Approving a call whose need is the network grants it.
                let network_allowed = network_allowed
                    || network_unenforceable
                    || (network_by_profile_default && needs_internet);
                PolicyResolution::Ask(Box::new(PendingApproval {
                    request: ApprovalRequest {
                        id: ApprovalId::generate(),
                        turn_id: None,
                        call_id: call.id.to_string(),
                        agent_id: self.agent_id.clone(),
                        action_fingerprint: action_fingerprint(call),
                        tool: call.name.clone(),
                        risk,
                        description,
                        command: command_line.clone(),
                        paths: rule_paths,
                        grant,
                    },
                    signature: approval_signature(call),
                    write,
                    network_scope: if network_allowed {
                        leveler_execution::NetworkScope::Internet
                    } else {
                        network_scope.clone()
                    },
                    command_line,
                    scoped_paths,
                }))
            }
        }
    }

    async fn resolve_resource_request(
        &self,
        call: &ToolCall,
        ctx: &ToolContext,
    ) -> Result<Option<leveler_core::GrantRequest>, String> {
        use leveler_core::GrantRequest;
        let Some(tool) = self.registry.get(&call.name) else {
            return Ok(None);
        };
        // A command resolves its own typed target (a Git remote effect); other
        // tools expose host-resolved path bindings. Both become one exact
        // capability-resource request.
        let request = match tool.command_grant_request(&call.arguments, ctx).await? {
            Some(request) => request,
            None => {
                let Some(bindings) = tool.grant_bindings(&call.arguments, ctx).await? else {
                    return Ok(None);
                };
                let root = ctx
                    .execution
                    .workspace
                    .as_ref()
                    .ok_or("no project resource is attached")?
                    .root();
                GrantRequest {
                    project_identity: leveler_execution::resolve_project_identity_with_environment(
                        root,
                        &ctx.execution.environment,
                    )
                    .await?,
                    bindings,
                }
            }
        };
        // Reusable consent is offered only for consumers whose execution layer
        // binds the approved identity to the real side effect. Everything else
        // keeps exact-call approval.
        if !Self::execution_bound_request(&request) {
            return Ok(None);
        }
        Ok(Some(request))
    }

    /// Whether every binding is covered by a proven execution boundary, so a
    /// Session/Project grant is safe to offer for this action. A Git request is
    /// anchored by its approved remote; the accompanying repository bindings are
    /// covered by the same frozen repository identity. Filesystem object binding
    /// and frozen Git targets exist on unix only; Windows keeps exact-call
    /// approval rather than pretending the binding holds.
    fn execution_bound_request(request: &leveler_core::GrantRequest) -> bool {
        use leveler_core::{Capability, ResourceIdentity};
        if request.bindings.is_empty() {
            return false;
        }
        let all = |pred: fn(&Capability, &ResourceIdentity) -> bool| {
            request
                .bindings
                .iter()
                .all(|binding| pred(&binding.capability, &binding.resource))
        };
        let background = |capability: &Capability, resource: &ResourceIdentity| {
            matches!(
                (capability, resource),
                (
                    Capability::BackgroundTaskObserve | Capability::BackgroundTaskControl,
                    ResourceIdentity::BackgroundTask { .. }
                )
            )
        };
        if all(background) {
            return true;
        }
        if !cfg!(unix) {
            return false;
        }
        let filesystem = |capability: &Capability, resource: &ResourceIdentity| {
            matches!(
                (capability, resource),
                (
                    Capability::FilesystemRead
                        | Capability::FilesystemWrite
                        | Capability::FilesystemDelete
                        | Capability::CredentialRawRead
                        | Capability::CredentialRawWrite,
                    ResourceIdentity::FilesystemPath { .. }
                )
            )
        };
        if all(filesystem) {
            return true;
        }
        let git = |capability: &Capability, resource: &ResourceIdentity| match resource {
            ResourceIdentity::ConfiguredRemote { .. } => matches!(
                capability,
                Capability::RemoteRead | Capability::RemoteMutate | Capability::RemoteForce
            ),
            // A credential binding is execution-bound: the frozen Git target
            // re-resolves the credential and refuses when its incarnation
            // changed, so a Session/Project grant cannot outlive the secret.
            ResourceIdentity::Credential { .. } => matches!(capability, Capability::CredentialUse),
            ResourceIdentity::Repository { .. } => matches!(
                capability,
                Capability::RepositoryRead
                    | Capability::RepositoryMutate
                    | Capability::RepositoryMetadataWrite
                    | Capability::RepositoryConfigWrite
                    | Capability::RepositoryDestroy
            ),
            _ => false,
        };
        all(git)
            && request.bindings.iter().any(|binding| {
                matches!(&binding.resource, ResourceIdentity::ConfiguredRemote { .. })
            })
    }

    /// The one place a decision is put to the reviewer and then the human
    /// (PR 5). Tool calls and `request_permissions` both come through here,
    /// so `--auto-approve`, session grants and the human-vs-headless
    /// distinction mean the same thing for both.
    ///
    /// A session grant or an "always" grant only skips the prompt; the policy
    /// the call runs under is whatever `pending` already carries.
    pub(crate) async fn ask(
        &self,
        pending: &PendingApproval,
        session_approved: Option<&mut HashSet<String>>,
        pending_always: Option<&mut Option<PendingStandingGrant>>,
        cancellation: &CancellationToken,
    ) -> AskOutcome {
        let human_only = pending.request.requires_human_consent();
        if human_only && !self.approver.has_human() {
            return AskOutcome::DeniedUnattended(
                "this consent action requires an actual human approver".into(),
            );
        }
        let mut session_approved = session_approved;
        if !human_only
            && session_approved
                .as_ref()
                .is_some_and(|set| set.contains(&pending.signature))
        {
            return AskOutcome::Allowed(AuthorizationEvidence::SessionGrant {
                signature: pending.signature.clone(),
            });
        }
        let review = if human_only {
            ReviewVerdict::NeedUser
        } else {
            tokio::select! {
                biased;
                _ = cancellation.cancelled() => return AskOutcome::Cancelled,
                verdict = self.auto_reviewer.review(&pending.request) => verdict,
            }
        };
        match review {
            ReviewVerdict::Allow => return AskOutcome::Allowed(AuthorizationEvidence::Reviewer),
            ReviewVerdict::Deny(reason) => return AskOutcome::DeniedUnattended(reason),
            ReviewVerdict::NeedUser => {}
        }
        let decision = tokio::select! {
            biased;
            _ = cancellation.cancelled() => return AskOutcome::Cancelled,
            decision = self.approver.decide_or_supersede(&pending.request) => decision,
        };
        let decision = match decision {
            ApprovalOutcome::Decided(decision) => decision,
            // The profile changed while this question waited. Do NOT record a
            // grant, a session approval or a standing rule: re-resolution is
            // the caller's job, and on Full it will resolve to Allow.
            ApprovalOutcome::Superseded => return AskOutcome::Superseded,
        };
        if human_only {
            return match decision {
                ApprovalDecision::Deny => AskOutcome::DeniedByUser,
                _ => AskOutcome::Allowed(AuthorizationEvidence::ApprovedOnce),
            };
        }
        if let Some(request) = &pending.request.grant {
            let scope = match decision {
                ApprovalDecision::ApproveOnce => leveler_core::GrantScope::Once,
                ApprovalDecision::ApproveSession => leveler_core::GrantScope::Session,
                ApprovalDecision::ApproveProject => leveler_core::GrantScope::Project,
                ApprovalDecision::ApproveAlways => {
                    return AskOutcome::DeniedUnattended(
                        "legacy rule approval cannot create a resource grant".into(),
                    );
                }
                ApprovalDecision::Deny => return AskOutcome::DeniedByUser,
            };
            if scope != leveler_core::GrantScope::Once {
                let Some(slot) = pending_always else {
                    return AskOutcome::DeniedUnattended(
                        "this execution surface cannot persist a resource grant".into(),
                    );
                };
                *slot = Some(PendingStandingGrant::Resource {
                    request: request.clone(),
                    scope,
                });
            }
            return AskOutcome::Allowed(AuthorizationEvidence::ResourceGrant {
                request: request.clone(),
                scope,
            });
        }
        match decision {
            ApprovalDecision::ApproveProject => AskOutcome::DeniedUnattended(
                "project approval requires a resolved resource identity".into(),
            ),
            ApprovalDecision::ApproveOnce => {
                AskOutcome::Allowed(AuthorizationEvidence::ApprovedOnce)
            }
            ApprovalDecision::ApproveSession => {
                // Strictly session-scoped: durable standing permission
                // is ApproveAlways writing a permission rule.
                if let Some(set) = &mut session_approved {
                    set.insert(pending.signature.clone());
                }
                AskOutcome::Allowed(AuthorizationEvidence::SessionGrant {
                    signature: pending.signature.clone(),
                })
            }
            ApprovalDecision::ApproveAlways => {
                // Do NOT write the standing permission here. The decision is
                // only queued for persistence at this point; writing a durable
                // rule now means a crash can leave a permanent grant in place
                // while the event log still shows the approval unresolved.
                // `admit` writes it after the barrier confirms the resolution
                // landed.
                if let Some(slot) = pending_always {
                    *slot = Some(PendingStandingGrant::Legacy {
                        tool: pending.request.tool.clone(),
                        command_line: pending.command_line.clone(),
                        paths: pending.scoped_paths.clone(),
                    });
                }
                // Session grant too: the current action proceeds even when no
                // durable rule could be persisted.
                if let Some(set) = &mut session_approved {
                    set.insert(pending.signature.clone());
                }
                AskOutcome::Allowed(AuthorizationEvidence::ApprovedAlways)
            }
            // Nobody was asked, so nobody refused. Reporting this as a user
            // decision teaches the model that this user rejects things they
            // never saw.
            ApprovalDecision::Deny if !self.approver.has_human() => AskOutcome::DeniedUnattended(
                "no approver was available in this non-interactive run (nobody declined it); \
                 re-run with --permission full-access to allow it"
                    .to_string(),
            ),
            ApprovalDecision::Deny => AskOutcome::DeniedByUser,
        }
    }

    #[cfg(test)]
    pub(crate) async fn authorize(
        &self,
        call: &ToolCall,
        session_approved: &mut HashSet<String>,
    ) -> Result<(), String> {
        let mut pending = None;
        let ctx = self.tool_context.clone();
        let result = self
            .authorize_with_cancellation(
                call,
                &ctx,
                session_approved,
                &mut pending,
                &CancellationToken::new(),
            )
            .await
            .map(|_| ());
        // The inline tests assert on the durable rule file, so apply what a
        // real run would apply after its barrier.
        if let Some(grant) = pending {
            match grant {
                PendingStandingGrant::Legacy {
                    tool,
                    command_line,
                    paths,
                } => self.remember_always(&tool, command_line.as_deref(), &paths),
                PendingStandingGrant::Resource { request, scope } => self
                    .resource_grants
                    .grant(
                        &request.project_identity,
                        self.grant_session_id(),
                        scope,
                        &request.bindings,
                    )
                    .await
                    .map_err(|e| e.to_string())?,
            }
        }
        result
    }

    /// Explain a denial that came from a context with no human in it, and — for
    /// a `remember` proposal — keep the content instead of discarding it.
    ///
    /// `pending/` exists for exactly this: consent deferred, not refused. The
    /// candidate never becomes active without an explicit `leveler memory
    /// accept`, so K36 still holds.
    fn park_unattended_denial(&self, call: &ToolCall) -> String {
        const UNATTENDED: &str =
            "no approver was available in this non-interactive run (nobody declined it)";
        if call.name != "remember" {
            return format!("{UNATTENDED}; re-run with --permission full-access to allow it");
        }
        let Some(root) = self.memory_root.as_ref() else {
            return format!("{UNATTENDED}; memory is not configured, so it could not be parked");
        };
        let title = call.arguments.get("title").and_then(|v| v.as_str());
        let body = call.arguments.get("body").and_then(|v| v.as_str());
        let (Some(title), Some(body)) = (title, body) else {
            return format!("{UNATTENDED}; the proposal had no title/body to park");
        };
        let tags = call
            .arguments
            .get("tags")
            .and_then(|v| v.as_array())
            .map(|items| {
                items
                    .iter()
                    .filter_map(|t| t.as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default();

        let parked = leveler_memory::MemoryStore::open(root)
            .map_err(|e| e.to_string())
            .and_then(|store| {
                let candidate = leveler_memory::MemoryCandidate::new(
                    title,
                    body,
                    leveler_memory::CandidateKind::Preference,
                    None,
                    leveler_memory::CandidateSource::SystemPropose,
                    tags,
                )
                .map_err(|e| e.to_string())?;
                store.propose(candidate).map_err(|e| e.to_string())
            });
        match parked {
            // Deliberately NOT "run `leveler memory accept <id>`": this text is
            // a tool result the MODEL reads, and a real run followed that
            // instruction — escalating the filesystem to adopt its own
            // candidate. The gate in `resolve_policy` now refuses that command,
            // and this message no longer suggests it either. Telling the USER
            // how to adopt it belongs on a user-facing channel.
            Ok(leveler_memory::ProposeOutcome::Pending(candidate)) => format!(
                "{UNATTENDED}; kept as a pending candidate [{}] for the user to review. \
                 Adopting a memory is their decision, not this run's — report that it is \
                 waiting and move on.",
                candidate.id
            ),
            // Already pending or suppressed: nothing lost either way.
            Ok(_) => format!("{UNATTENDED}; this memory is already awaiting your review"),
            Err(error) => format!("{UNATTENDED}; parking it failed: {error}"),
        }
    }

    /// Persist an `ApproveAlways` decision as project permission rules and
    /// extend the live rule set. Calls that cannot be expressed as a safe
    /// rule (shell scripts, memory writes, other tools) derive no rules and
    /// stay session-only; so does a missing rules path. Persistence failures
    /// are logged, never fatal — the user already approved this action.
    pub(crate) fn remember_always(&self, tool: &str, command_line: Option<&str>, paths: &[String]) {
        let rules = leveler_execution::always_rules_for(tool, command_line, paths);
        if rules.is_empty() {
            return;
        }
        let Some(path) = &self.permission_rules_path else {
            tracing::warn!(
                tool,
                "approve-always without a project rules path; grant stays session-only"
            );
            return;
        };
        for rule in &rules {
            if let Err(e) = leveler_execution::append_rule_file(path, rule) {
                tracing::warn!(tool, error = %e, "could not persist permission rule");
            }
        }
        self.permission_rules
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .extend(leveler_execution::PermissionRuleSet::from_rules(rules));
    }

    /// Execute one admitted call, returning `(content, is_error, image,
    /// workspace_snapshot, plan, modified paths, executed commands, applied
    /// diff)` to feed
    /// back to the model. Infrastructure errors are converted to model-visible
    /// text so the model can react rather than the loop aborting. Also records
    /// any files the tool modified and the commands it actually ran.
    pub(crate) async fn dispatch(
        &self,
        admitted: &AdmittedCall,
        modified_files: &mut Vec<String>,
        cancellation: &CancellationToken,
        output: Option<tokio::sync::mpsc::Sender<leveler_execution::OutputChunk>>,
    ) -> (
        String,
        bool,
        Option<ContentPart>,
        Option<String>,
        Option<Vec<PlanStep>>,
        Vec<String>,
        Vec<Vec<String>>,
        Option<String>,
        super::dispatch::CommandFacts,
    ) {
        let (content, is_error, metadata) = self.dispatch_raw(admitted, cancellation, output).await;
        // The call's own modified paths, BEFORE merging into the epoch set:
        // a re-edit of an already-modified file is invisible in the merged
        // list, and the caller needs to know the call mutated at all (R011-F1).
        let mut call_files = Vec::new();
        collect_modified(&metadata, &mut call_files);
        for path in &call_files {
            if !modified_files.iter().any(|existing| existing == path) {
                modified_files.push(path.clone());
            }
        }
        let image = extract_image(&metadata);
        let snapshot = metadata
            .get("workspace_snapshot")
            .and_then(serde_json::Value::as_str)
            .map(ToOwned::to_owned);
        let plan = extract_plan(&metadata);
        let executed = extract_executed_commands(&metadata);
        let applied_diff = extract_applied_diff(&metadata);
        let command = super::dispatch::extract_command_facts(&metadata);
        (
            content,
            is_error,
            image,
            snapshot,
            plan,
            call_files,
            executed,
            applied_diff,
            command,
        )
    }

    /// Execute one already-narrowed read-only call for a host-owned surface
    /// that is not a turn (`/btw`). It runs through the SAME admission and
    /// execution pipeline as every other call, so no caller reaches the
    /// execution entry directly; the surface is observe-class only, so the
    /// pipeline reaches no mutating path.
    pub async fn run_read_only_call(
        &self,
        call: ToolCall,
        ctx: ToolContext,
        cancellation: &CancellationToken,
    ) -> Result<(String, bool), AgentError> {
        let mut session_approved = HashSet::new();
        // `parallel = true`: the surface is side-effect-free by declaration, so
        // there is no side effect for a crash to lose and no barrier to wait on.
        let admitted = self
            .admit(call, ctx, true, &mut session_approved, cancellation)
            .await
            .map_err(|error| match error {
                AdmitError::Fatal(error) => error,
                AdmitError::Refused { call, reason } => {
                    AgentError::Model(leveler_model::ModelError::new(
                        leveler_model::ModelErrorKind::InvalidRequest,
                        format!("tool `{}` refused: {reason}", call.name),
                    ))
                }
            })?;
        let mut modified_files = Vec::new();
        let (content, is_error, ..) = self
            .dispatch(&admitted, &mut modified_files, cancellation, None)
            .await;
        Ok((content, is_error))
    }

    /// Execute one admitted call, returning `(content, is_error, metadata)`
    /// without touching shared state — safe to run concurrently for
    /// parallel-safe tools. The caller folds `metadata` (modified files,
    /// images) back in call order.
    pub(crate) async fn dispatch_raw(
        &self,
        admitted: &AdmittedCall,
        cancellation: &CancellationToken,
        output: Option<tokio::sync::mpsc::Sender<leveler_execution::OutputChunk>>,
    ) -> (String, bool, serde_json::Value) {
        let call = &admitted.call;
        // The tool runs under the policy admission froze — the one place the
        // decision and the execution are tied together (PR 5).
        let resolved = admitted.resolved();
        tracing::debug!(
            tool = %call.name,
            write_scope = ?resolved.write,
            network_scope = ?resolved.network_scope,
            authorization = ?resolved.authorization,
            "executing admitted call"
        );
        // This call's own cancellation: a child of the turn's, handed to the
        // host so a user can stop this one call without stopping the turn.
        let call_cancel = cancellation.child_token();
        if let Some(host) = &self.steering {
            host.tool_call_started(call.id.as_str(), call_cancel.clone());
        }
        let mut ctx = admitted.execution_context().clone();
        ctx.output = output;
        let executed = self
            .registry
            .execute(&call.name, call.arguments.clone(), ctx, call_cancel)
            .await;
        if let Some(host) = &self.steering {
            host.tool_call_ended(call.id.as_str());
        }
        let outcome = match executed {
            Ok(output) => (output.content, output.is_error, output.metadata),
            // A stopped command reports HOW it stopped, so a client shows
            // "stopped" only when the process tree is proven gone. Every process
            // failure also carries the machine-readable execution status, so a
            // consumer can tell "the tool could not run" from "the command ran
            // and exited non-zero" without parsing the message.
            Err(ToolError::Process(e)) => {
                let status = e.execution_status();
                let stop = match &e {
                    leveler_execution::ProcessError::Cancelled => {
                        Some(leveler_execution::CommandStop::Confirmed)
                    }
                    leveler_execution::ProcessError::CancelUnconfirmed => {
                        Some(leveler_execution::CommandStop::Unconfirmed)
                    }
                    _ => None,
                };
                let metadata = match stop {
                    Some(stop) => {
                        serde_json::json!({ "execution_status": status, "stop": stop })
                    }
                    None => serde_json::json!({ "execution_status": status }),
                };
                (format!("tool error: {e}"), true, metadata)
            }
            Err(ToolError::NotFound(name)) if name == "task" => (
                "tool error: unsupported tool `task`; use `spawn_agent` for delegation".to_string(),
                true,
                serde_json::Value::Null,
            ),
            Err(e) => (format!("tool error: {e}"), true, serde_json::Value::Null),
        };
        // F6 SECURITY BOUNDARY. Every tool result — read_file, shell, browser,
        // web, MCP, git — passes through this one function exactly once, so
        // sanitizing here keeps concrete secret values out of the model's
        // context AND out of the provider request, instead of only scrubbing
        // them on the way to the database (which is what let R007's agent see
        // a credential, paraphrase it, and persist the plaintext anyway).
        // Values found are remembered for this session so the same plaintext
        // can be scrubbed if it reappears in durable text from another path.
        let outcome = {
            let (content, is_error, metadata) = outcome;
            let (sanitized, found) = leveler_core::sanitize_model_visible(&content);
            if !found.is_empty() {
                if let Some(session) = admitted.ctx.session_scope.as_deref() {
                    leveler_core::register_session_secrets(session, &found);
                }
                // Names only — never the values.
                tracing::debug!(
                    tool = %call.name,
                    redacted = found.len(),
                    keys = ?found.iter().map(|f| f.key.as_str()).collect::<Vec<_>>(),
                    "redacted secret values from tool output before the model saw them"
                );
            }
            (sanitized, is_error, metadata)
        };
        // Untrusted-content boundary: origin/authority marker on output that
        // came from outside the runtime. Applied once, right after secret
        // sanitization and before the value reaches the model or the UI; it
        // wraps the body, it never rewrites it. Errors are host-generated
        // explanations, so only successful external content is marked.
        let outcome = {
            let (content, is_error, metadata) = outcome;
            let content = super::dispatch::mark_external_content(&call.name, content, is_error);
            (content, is_error, metadata)
        };

        // Close a delegated call's canonical record here, not in `dispatch`:
        // read-only tools run in the concurrent batch, which calls this
        // directly, so recording upstream would leave every parallel child
        // call looking permanently dangling.
        if let (Some(barrier), Some(agent_id)) = (&self.event_barrier, &self.agent_id) {
            barrier.record_child_tool_event(super::ChildToolEvent::Finished {
                agent_id: agent_id.clone(),
                call_id: call.id.as_str().to_string(),
                name: call.name.clone(),
                is_error: outcome.1,
                preview: super::dispatch::preview(&outcome.0),
            });
        }
        outcome
    }
}

#[cfg(test)]
mod authorize_tests {
    use super::*;
    use std::sync::Arc;

    use leveler_core::ToolCallId;
    use leveler_execution::{Approver, PermissionProfile, Workspace};
    use leveler_model::{
        ModelError, ModelEventStream, ModelProfile, ModelRef, ModelRequest, ModelResponse,
        ModelRuntime,
    };
    use leveler_tools::{ToolContext, default_registry};

    /// Runtime stub: authorize never queries the model.
    struct StubRuntime;

    #[async_trait::async_trait]
    impl ModelRuntime for StubRuntime {
        async fn generate(
            &self,
            _request: ModelRequest,
            _cancellation: CancellationToken,
        ) -> Result<ModelResponse, ModelError> {
            unreachable!("authorize never queries the model")
        }

        async fn stream(
            &self,
            _request: ModelRequest,
            _cancellation: CancellationToken,
        ) -> Result<ModelEventStream, ModelError> {
            unreachable!("authorize never queries the model")
        }

        async fn profile(&self, _model: &ModelRef) -> Result<ModelProfile, ModelError> {
            unreachable!("authorize never queries the model")
        }
    }

    /// Approver stub returning a fixed decision and recording every request.
    struct FixedApprover {
        decision: ApprovalDecision,
        requests: std::sync::Mutex<Vec<ApprovalRequest>>,
    }

    impl FixedApprover {
        fn new(decision: ApprovalDecision) -> Self {
            Self {
                decision,
                requests: std::sync::Mutex::new(Vec::new()),
            }
        }

        fn asks(&self) -> usize {
            self.requests.lock().unwrap().len()
        }

        fn last_request(&self) -> Option<ApprovalRequest> {
            self.requests.lock().unwrap().last().cloned()
        }
    }

    #[async_trait::async_trait]
    impl Approver for FixedApprover {
        async fn decide(&self, request: &ApprovalRequest) -> ApprovalDecision {
            self.requests.lock().unwrap().push(request.clone());
            self.decision
        }
    }

    fn executor_for(dir: &std::path::Path, approver: Arc<FixedApprover>) -> Executor {
        let workspace = Workspace::new(dir).unwrap();
        let tool_context = ToolContext::new(workspace, PermissionProfile::Assisted);
        Executor::new(
            Arc::new(StubRuntime),
            Arc::new(default_registry()),
            tool_context,
            ModelRef::new("mock", "m"),
            10,
        )
        .with_approver(approver)
    }

    /// Same, at an explicit permission profile.
    fn executor_at(
        dir: &std::path::Path,
        mode: PermissionProfile,
        approver: Arc<FixedApprover>,
    ) -> Executor {
        let workspace = Workspace::new(dir).unwrap();
        let tool_context = ToolContext::new(workspace, mode);
        Executor::new(
            Arc::new(StubRuntime),
            Arc::new(default_registry()),
            tool_context,
            ModelRef::new("mock", "m"),
            10,
        )
        .with_approver(approver)
    }

    /// An executor under the read-only overlay (`leveler plan` / plan
    /// collaboration): Safe tools only, whatever the profile allows.
    fn read_only_executor(dir: &std::path::Path, approver: Arc<FixedApprover>) -> Executor {
        let workspace = Workspace::new(dir).unwrap();
        let tool_context =
            ToolContext::new(workspace, PermissionProfile::Assisted).with_read_only(true);
        Executor::new(
            Arc::new(StubRuntime),
            Arc::new(default_registry()),
            tool_context,
            ModelRef::new("mock", "m"),
            10,
        )
        .with_approver(approver)
    }

    #[tokio::test]
    async fn auto_read_only_action_asks_once_executes_and_does_not_elevate_next_call() {
        let dir = tempfile::tempdir().unwrap();
        let approver = Arc::new(FixedApprover::new(ApprovalDecision::ApproveOnce));
        let executor = read_only_executor(dir.path(), approver.clone());
        let action = call(
            "apply_patch",
            serde_json::json!({"patch":"*** Begin Patch\n*** Add File: approved.txt\naccepted\n*** End Patch"}),
        );
        assert!(matches!(
            resolve(&executor, &action).await,
            PolicyResolution::Ask(_)
        ));
        let admitted = executor
            .admit(
                action.clone(),
                executor.tool_context.clone(),
                false,
                &mut HashSet::new(),
                &CancellationToken::new(),
            )
            .await
            .ok()
            .expect("human can approve Auto readonly action");
        let (_, error, _) = executor
            .dispatch_raw(&admitted, &CancellationToken::new(), None)
            .await;
        assert!(!error);
        assert_eq!(
            std::fs::read_to_string(dir.path().join("approved.txt")).unwrap(),
            "accepted\n"
        );
        assert_eq!(approver.asks(), 1);
        assert!(executor.tool_context.policy.read_only);
        executor
            .authorize(&read_file_call(), &mut HashSet::new())
            .await
            .unwrap();
        assert!(matches!(
            resolve(&executor, &action).await,
            PolicyResolution::Ask(_)
        ));
        executor
            .tool_context
            .policy
            .permission_profile()
            .set(PermissionProfile::RequestApproval);
        assert!(matches!(
            resolve(&executor, &action).await,
            PolicyResolution::Deny(_)
        ));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn auto_self_consent_requires_real_human_then_executes_exact_call() {
        struct AllowAll;
        #[async_trait::async_trait]
        impl leveler_execution::AutoReviewer for AllowAll {
            async fn review(&self, _: &ApprovalRequest) -> ReviewVerdict {
                ReviewVerdict::Allow
            }
        }
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let executable = dir.path().join("leveler");
        let marker = dir.path().join("human-marker");
        std::fs::write(&executable, "#!/bin/sh\nprintf approved > \"$3\"\n").unwrap();
        std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o700)).unwrap();
        let approver = Arc::new(FixedApprover::new(ApprovalDecision::ApproveOnce));
        let executor =
            executor_for(dir.path(), approver.clone()).with_auto_reviewer(Arc::new(AllowAll));
        let action = call(
            "run_command",
            serde_json::json!({"program":executable,"args":["memory","accept",marker]}),
        );
        assert!(matches!(
            resolve(&executor, &action).await,
            PolicyResolution::Ask(_)
        ));
        let mut sessions = HashSet::new();
        let admitted = executor
            .admit(
                action.clone(),
                executor.tool_context.clone(),
                false,
                &mut sessions,
                &CancellationToken::new(),
            )
            .await
            .ok()
            .expect("human approval admits self-consent exact command");
        assert!(matches!(
            admitted.resolved().authorization,
            AuthorizationEvidence::ApprovedOnce
        ));
        let (_, error, _) = executor
            .dispatch_raw(&admitted, &CancellationToken::new(), None)
            .await;
        assert!(!error);
        assert_eq!(std::fs::read_to_string(marker).unwrap(), "approved");
        assert_eq!(
            approver.asks(),
            1,
            "auto reviewer must not approve self consent"
        );
        assert!(sessions.is_empty());
        assert!(matches!(
            resolve(&executor, &action).await,
            PolicyResolution::Ask(_)
        ));
        let unattended = executor_for(
            dir.path(),
            Arc::new(FixedApprover::new(ApprovalDecision::Deny)),
        )
        .with_auto_reviewer(Arc::new(AllowAll))
        .with_approver(Arc::new(leveler_execution::AutoApprove));
        assert!(
            unattended
                .authorize(&action, &mut HashSet::new())
                .await
                .is_err()
        );
        executor
            .tool_context
            .policy
            .permission_profile()
            .set(PermissionProfile::RequestApproval);
        assert!(matches!(
            resolve(&executor, &action).await,
            PolicyResolution::Deny(_)
        ));
    }

    #[tokio::test]
    async fn a_network_denied_run_refuses_an_mcp_tool_instead_of_pretending() {
        let dir = tempfile::tempdir().unwrap();
        let approver = Arc::new(FixedApprover::new(ApprovalDecision::ApproveOnce));
        let workspace = Workspace::new(dir.path()).unwrap();
        // An explicit Restricted denial cannot be implemented by MCP.
        let tool_context =
            ToolContext::new(workspace, PermissionProfile::RequestApproval).with_sandbox(true);
        let executor = Executor::new(
            Arc::new(StubRuntime),
            Arc::new(default_registry()),
            tool_context,
            ModelRef::new("mock", "m"),
            10,
        )
        .with_approver(approver.clone());

        let call = ToolCall {
            id: ToolCallId::new("m"),
            name: "mcp__fs__read".to_string(),
            arguments: serde_json::json!({}),
        };
        let reason = executor
            .authorize(&call, &mut HashSet::new())
            .await
            .expect_err("a denial this runtime cannot enforce must refuse the call");
        assert!(
            reason.contains("network access is denied"),
            "the refusal must name the unenforceable denial: {reason}"
        );
        assert_eq!(approver.asks(), 0, "not a question for the user");
    }

    fn read_file_call() -> ToolCall {
        ToolCall {
            id: ToolCallId::new("r"),
            name: "read_file".to_string(),
            arguments: serde_json::json!({"path": "Cargo.toml"}),
        }
    }

    /// An ordinary read-only repository search — the exact shape the user saw
    /// a child stop on.
    fn grep_call() -> ToolCall {
        ToolCall {
            id: ToolCallId::new("g"),
            name: "run_command".to_string(),
            arguments: serde_json::json!({"program": "grep", "args": ["-rn", "foo", "src"]}),
        }
    }

    /// An executor bound to a permission profile the caller owns — the shape
    /// the daemon builds, where the session holds the cell.
    fn executor_sharing(
        dir: &std::path::Path,
        profile: &leveler_execution::SharedPermissionProfile,
        approver: Arc<FixedApprover>,
    ) -> Executor {
        let workspace = Workspace::new(dir).unwrap();
        let tool_context =
            ToolContext::new(workspace, profile.get()).with_permission_profile(profile.clone());
        Executor::new(
            Arc::new(StubRuntime),
            Arc::new(default_registry()),
            tool_context,
            ModelRef::new("mock", "m"),
            10,
        )
        .with_approver(approver)
    }

    /// Same as [`executor_sharing`], with an approver that needs to report a
    /// supersession (the plain entry point only carries decisions).
    fn executor_sharing_any(
        dir: &std::path::Path,
        profile: &leveler_execution::SharedPermissionProfile,
        approver: Arc<dyn Approver>,
    ) -> Executor {
        let workspace = Workspace::new(dir).unwrap();
        let tool_context =
            ToolContext::new(workspace, profile.get()).with_permission_profile(profile.clone());
        Executor::new(
            Arc::new(StubRuntime),
            Arc::new(default_registry()),
            tool_context,
            ModelRef::new("mock", "m"),
            10,
        )
        .with_approver(approver)
    }

    /// An approver that reports one supersession — after switching the session
    /// to `switch_to` — and then answers normally. Models the lifecycle the app
    /// channel approver reports when `SetPermissionProfile` voids a question.
    struct SupersedeOnce {
        session: leveler_execution::SharedPermissionProfile,
        switch_to: PermissionProfile,
        calls: std::sync::atomic::AtomicUsize,
    }

    #[async_trait::async_trait]
    impl Approver for SupersedeOnce {
        async fn decide(&self, _: &ApprovalRequest) -> ApprovalDecision {
            unreachable!("the executor must call decide_or_supersede")
        }

        async fn decide_or_supersede(
            &self,
            _: &ApprovalRequest,
        ) -> leveler_execution::ApprovalOutcome {
            let call = self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            if call == 0 {
                self.session.set(self.switch_to);
                leveler_execution::ApprovalOutcome::Superseded
            } else {
                leveler_execution::ApprovalOutcome::Decided(ApprovalDecision::ApproveOnce)
            }
        }
    }

    /// A supersession caused by switching to Full must re-resolve to Allow and
    /// must NOT ask a second time: nobody decided, and Full needs no decision.
    #[tokio::test]
    async fn superseded_into_full_re_resolves_to_allow_without_a_second_ask() {
        let dir = tempfile::tempdir().unwrap();
        let session = leveler_execution::SharedPermissionProfile::new(PermissionProfile::Assisted);
        let approver = Arc::new(SupersedeOnce {
            session: session.clone(),
            switch_to: PermissionProfile::FullAccess,
            calls: std::sync::atomic::AtomicUsize::new(0),
        });
        let exec = executor_sharing_any(dir.path(), &session, approver.clone());
        let action = call(
            "run_command",
            serde_json::json!({"program": "rm", "args": ["-rf", "x"]}),
        );
        let mut session_approved = HashSet::new();
        let mut pending_always = None;
        let resolved = exec
            .authorize_with_cancellation(
                &action,
                &exec.tool_context.clone(),
                &mut session_approved,
                &mut pending_always,
                &CancellationToken::new(),
            )
            .await
            .expect("Full must allow the re-resolved call");
        assert!(resolved.unrestricted_execution());
        assert_eq!(
            approver.calls.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "Full must resolve without asking again"
        );
        assert!(pending_always.is_none(), "a supersession persists no grant");
        assert!(
            session_approved.is_empty(),
            "a supersession grants no session"
        );
    }

    /// The security-relevant direction: a supersession caused by a STRICTER
    /// profile must re-ask under that profile instead of riding the old answer.
    #[tokio::test]
    async fn superseded_into_a_stricter_profile_asks_again() {
        let dir = tempfile::tempdir().unwrap();
        let session = leveler_execution::SharedPermissionProfile::new(PermissionProfile::Assisted);
        let approver = Arc::new(SupersedeOnce {
            session: session.clone(),
            switch_to: PermissionProfile::RequestApproval,
            calls: std::sync::atomic::AtomicUsize::new(0),
        });
        let exec = executor_sharing_any(dir.path(), &session, approver.clone());
        let action = call(
            "run_command",
            serde_json::json!({"program": "rm", "args": ["-rf", "x"]}),
        );
        let mut session_approved = HashSet::new();
        let mut pending_always = None;
        exec.authorize_with_cancellation(
            &action,
            &exec.tool_context.clone(),
            &mut session_approved,
            &mut pending_always,
            &CancellationToken::new(),
        )
        .await
        .expect("the second, real decision allows the call");
        assert_eq!(
            approver.calls.load(std::sync::atomic::Ordering::SeqCst),
            2,
            "a stricter profile must re-ask, not reuse the superseded question"
        );
    }

    /// The defect this whole change exists for: the user switches to 完全访问
    /// while a turn is running, and that turn keeps prompting because it
    /// copied the profile at startup. The SAME executor — no restart — must
    /// obey the change at its next decision.
    #[tokio::test]
    async fn a_running_agent_obeys_a_switch_to_full_access() {
        let dir = tempfile::tempdir().unwrap();
        let session = leveler_execution::SharedPermissionProfile::new(PermissionProfile::Assisted);
        let approver = Arc::new(FixedApprover::new(ApprovalDecision::ApproveOnce));
        let main = executor_sharing(dir.path(), &session, approver.clone());
        let mut seen = HashSet::new();

        main.authorize(&rm_rf_call(), &mut seen).await.unwrap();
        assert_eq!(
            approver.asks(),
            1,
            "替我审批 asks for an irreversible delete"
        );

        session.set(PermissionProfile::FullAccess);

        // Same executor, same turn, a call the session cache cannot answer.
        let mut fresh = HashSet::new();
        main.authorize(&rm_rf_call(), &mut fresh).await.unwrap();
        assert_eq!(
            approver.asks(),
            1,
            "the switch must land on the next decision, not the next turn"
        );
    }

    /// The half that is a security property rather than a convenience one:
    /// tightening must be just as live as widening, or a user who revokes
    /// 完全访问 keeps an agent running with authority they just took away.
    #[tokio::test]
    async fn a_running_agent_obeys_a_switch_away_from_full_access() {
        let dir = tempfile::tempdir().unwrap();
        let session =
            leveler_execution::SharedPermissionProfile::new(PermissionProfile::FullAccess);
        let approver = Arc::new(FixedApprover::new(ApprovalDecision::ApproveOnce));
        let main = executor_sharing(dir.path(), &session, approver.clone());
        let mut seen = HashSet::new();

        main.authorize(&rm_rf_call(), &mut seen).await.unwrap();
        assert_eq!(approver.asks(), 0, "完全访问 does not prompt");

        session.set(PermissionProfile::Assisted);

        let mut fresh = HashSet::new();
        main.authorize(&rm_rf_call(), &mut fresh).await.unwrap();
        assert_eq!(
            approver.asks(),
            1,
            "revoked authority must not survive in a turn that is already running"
        );
    }

    /// A child delegated BEFORE the switch is the case a per-turn snapshot
    /// gets most wrong: it holds a clone of the parent's context, so it must
    /// hold a reference to the same cell rather than a copy of its value.
    #[tokio::test]
    async fn a_child_delegated_before_the_switch_sees_it_too() {
        let dir = tempfile::tempdir().unwrap();
        let session = leveler_execution::SharedPermissionProfile::new(PermissionProfile::Assisted);
        let approver = Arc::new(FixedApprover::new(ApprovalDecision::ApproveOnce));
        let main = executor_sharing(dir.path(), &session, approver.clone());

        // Delegated while the task is still 替我审批.
        let child = main
            .child_for_role_on(crate::sub_agent::AgentRole::Default, Vec::new(), None)
            .with_agent_id("child-before");
        let mut seen = HashSet::new();
        child.authorize(&rm_rf_call(), &mut seen).await.unwrap();
        assert_eq!(approver.asks(), 1);

        session.set(PermissionProfile::FullAccess);
        let mut fresh = HashSet::new();
        child.authorize(&rm_rf_call(), &mut fresh).await.unwrap();
        assert_eq!(
            approver.asks(),
            1,
            "the existing child must see the upgrade"
        );

        session.set(PermissionProfile::Assisted);
        let mut again = HashSet::new();
        child.authorize(&rm_rf_call(), &mut again).await.unwrap();
        assert_eq!(
            approver.asks(),
            2,
            "and the downgrade, without being respawned"
        );
    }

    /// A child delegated AFTER a change starts from the current profile, in
    /// both directions.
    #[tokio::test]
    async fn a_child_delegated_after_the_switch_starts_from_the_current_profile() {
        let dir = tempfile::tempdir().unwrap();
        let session = leveler_execution::SharedPermissionProfile::new(PermissionProfile::Assisted);
        let approver = Arc::new(FixedApprover::new(ApprovalDecision::ApproveOnce));
        let main = executor_sharing(dir.path(), &session, approver.clone());

        session.set(PermissionProfile::FullAccess);
        let after_upgrade = main
            .child_for_role_on(crate::sub_agent::AgentRole::Default, Vec::new(), None)
            .with_agent_id("child-after-upgrade");
        let mut seen = HashSet::new();
        after_upgrade
            .authorize(&rm_rf_call(), &mut seen)
            .await
            .unwrap();
        assert_eq!(approver.asks(), 0);

        session.set(PermissionProfile::Assisted);
        let after_downgrade = main
            .child_for_role_on(crate::sub_agent::AgentRole::Default, Vec::new(), None)
            .with_agent_id("child-after-downgrade");
        let mut fresh = HashSet::new();
        after_downgrade
            .authorize(&rm_rf_call(), &mut fresh)
            .await
            .unwrap();
        assert_eq!(approver.asks(), 1);
    }

    #[tokio::test]
    async fn full_access_bypasses_child_role_and_claim_permissions() {
        let dir = tempfile::tempdir().unwrap();
        let session =
            leveler_execution::SharedPermissionProfile::new(PermissionProfile::FullAccess);
        let approver = Arc::new(FixedApprover::new(ApprovalDecision::Deny));
        let main = executor_sharing(dir.path(), &session, approver.clone());
        for role in [
            crate::sub_agent::AgentRole::Explorer,
            crate::sub_agent::AgentRole::Default,
        ] {
            let child = main
                .child_for_role_on(role, Vec::new(), None)
                .with_agent_id("unclaimed");
            assert!(child.registry.mutates_files("apply_patch"));
            let action = call(
                "apply_patch",
                serde_json::json!({"patch":"*** Begin Patch\n*** Add File: full.txt\n+x\n*** End Patch"}),
            );
            match resolve(&child, &action).await {
                PolicyResolution::Allow(policy) => assert!(policy.unrestricted_execution()),
                other => panic!("{other:?}"),
            }
        }
        assert_eq!(approver.asks(), 0);
    }

    #[tokio::test]
    async fn full_host_opener_bypasses_a_childs_owned_scope() {
        let dir = tempfile::tempdir().unwrap();
        let session =
            leveler_execution::SharedPermissionProfile::new(PermissionProfile::FullAccess);
        let main = executor_sharing(
            dir.path(),
            &session,
            Arc::new(FixedApprover::new(ApprovalDecision::ApproveOnce)),
        );
        let child = main
            .child_for_role_on(crate::sub_agent::AgentRole::Default, Vec::new(), None)
            .with_agent_id("opener");
        child
            .ownership
            .try_claim("opener", &["owned".to_string()])
            .unwrap();
        let call = ToolCall {
            id: ToolCallId::new("open-owned"),
            name: "run_command".into(),
            arguments: serde_json::json!({"program":"open", "args":["owned"]}),
        };
        let result = child
            .admit(
                call,
                child.tool_context.clone(),
                false,
                &mut HashSet::new(),
                &CancellationToken::new(),
            )
            .await;
        assert!(
            result.is_ok(),
            "Full host opener must bypass task scope permissions"
        );
    }

    /// Several children already in flight all share the session cell, so one
    /// write is visible to every next authorization — nothing to broadcast.
    #[tokio::test]
    async fn every_existing_child_sees_the_same_live_switch() {
        let dir = tempfile::tempdir().unwrap();
        let session = leveler_execution::SharedPermissionProfile::new(PermissionProfile::Assisted);
        let approver = Arc::new(FixedApprover::new(ApprovalDecision::ApproveOnce));
        let main = executor_sharing(dir.path(), &session, approver.clone());
        let children: Vec<Executor> = (0..3)
            .map(|i| {
                main.child_for_role_on(crate::sub_agent::AgentRole::Default, Vec::new(), None)
                    .with_agent_id(format!("child-{i}"))
            })
            .collect();

        session.set(PermissionProfile::FullAccess);
        for child in &children {
            let mut seen = HashSet::new();
            child.authorize(&rm_rf_call(), &mut seen).await.unwrap();
        }
        assert_eq!(approver.asks(), 0, "every existing child observes Full");

        session.set(PermissionProfile::Assisted);
        for child in &children {
            let mut seen = HashSet::new();
            child.authorize(&rm_rf_call(), &mut seen).await.unwrap();
        }
        assert_eq!(
            approver.asks(),
            3,
            "and every existing child observes the downgrade"
        );
    }

    /// 完全访问 is a promise about the whole task, not about the top agent:
    /// the user opted out of prompting once, and a delegated agent doing
    /// ordinary work must not re-open that decision.
    #[tokio::test]
    async fn full_access_asks_nobody_in_the_main_agent_or_in_a_child() {
        let dir = tempfile::tempdir().unwrap();

        let main_approver = Arc::new(FixedApprover::new(ApprovalDecision::ApproveOnce));
        let main = executor_at(
            dir.path(),
            PermissionProfile::FullAccess,
            main_approver.clone(),
        );
        let mut session = HashSet::new();
        main.authorize(&grep_call(), &mut session)
            .await
            .expect("full access executes a repo search directly");
        assert_eq!(main_approver.asks(), 0, "main agent must not be asked");

        let child = main
            .child_for_role_on(crate::sub_agent::AgentRole::Explorer, Vec::new(), None)
            .with_agent_id("child-1");
        let mut child_session = HashSet::new();
        child
            .authorize(&grep_call(), &mut child_session)
            .await
            .expect("a child under full access executes the same search directly");
        assert_eq!(
            main_approver.asks(),
            0,
            "a delegated agent must not re-open a decision the user already made"
        );
    }

    /// `rm -rf …` classifies dangerous (irreversible destruction), so Assisted
    /// always asks for it. (`git push`, `git reset --hard` and `git clean -fd`
    /// ask too, through their own Git effects.)
    fn rm_rf_call() -> ToolCall {
        ToolCall {
            id: ToolCallId::new("c"),
            name: "run_command".to_string(),
            arguments: serde_json::json!({"program": "rm", "args": ["-rf", "scratch"]}),
        }
    }

    /// A headless approver that denies, standing in for `AutoApprove` in a
    /// non-interactive run (`leveler run`, CI, eval).
    struct HeadlessDeny;

    #[async_trait::async_trait]
    impl Approver for HeadlessDeny {
        fn has_human(&self) -> bool {
            false
        }
        async fn decide(&self, _request: &ApprovalRequest) -> ApprovalDecision {
            ApprovalDecision::Deny
        }
    }

    fn remember_call() -> ToolCall {
        ToolCall {
            id: ToolCallId::new("m"),
            name: "remember".to_string(),
            arguments: serde_json::json!({
                "title": "用 pnpm",
                "body": "本仓库统一用 pnpm，不要用 npm。",
            }),
        }
    }

    /// The model must not be told the user rejected something the user never
    /// saw — it reads that as a standing preference against memory.
    #[tokio::test]
    async fn a_headless_denial_is_not_reported_as_the_user_refusing() {
        let dir = tempfile::tempdir().unwrap();
        let workspace = Workspace::new(dir.path()).unwrap();
        let tool_context = ToolContext::new(workspace, PermissionProfile::Assisted);
        let executor = Executor::new(
            Arc::new(StubRuntime),
            Arc::new(default_registry()),
            tool_context,
            ModelRef::new("mock", "m"),
            10,
        )
        .with_memory_root(Some(dir.path().join("memory")))
        .with_approver(Arc::new(HeadlessDeny));
        let mut session = HashSet::new();

        let err = executor
            .authorize(&remember_call(), &mut session)
            .await
            .expect_err("assisted still gates memory writes");
        assert!(
            !err.contains("denied by user"),
            "nobody was asked, so nobody denied it: {err}"
        );
    }

    /// Discarding the proposal loses it for good. Parking it as a pending
    /// candidate is what `pending/` is for — consent deferred, not refused.
    #[tokio::test]
    async fn a_headless_run_parks_the_memory_instead_of_dropping_it() {
        let dir = tempfile::tempdir().unwrap();
        let memory_root = dir.path().join("memory");
        let workspace = Workspace::new(dir.path()).unwrap();
        let tool_context = ToolContext::new(workspace, PermissionProfile::Assisted);
        let executor = Executor::new(
            Arc::new(StubRuntime),
            Arc::new(default_registry()),
            tool_context,
            ModelRef::new("mock", "m"),
            10,
        )
        .with_memory_root(Some(memory_root.clone()))
        .with_approver(Arc::new(HeadlessDeny));
        let mut session = HashSet::new();

        let err = executor
            .authorize(&remember_call(), &mut session)
            .await
            .expect_err("the call itself still does not store active memory");
        // The message must let the model TELL the user something is waiting —
        // hence the candidate id — without handing it a command to adopt the
        // memory itself. The previous wording spelled out
        // `leveler memory accept <id>`, and a real run executed it.
        let store_peek = leveler_memory::MemoryStore::open(&memory_root).unwrap();
        let candidate_id = store_peek.list_pending().unwrap()[0].id.clone();
        assert!(
            err.contains(&candidate_id),
            "the message must name the waiting candidate: {err}"
        );
        assert!(
            !err.contains("leveler memory"),
            "the message must not hand the model a way to adopt it: {err}"
        );

        let store = leveler_memory::MemoryStore::open(&memory_root).unwrap();
        let pending = store.list_pending().unwrap();
        assert_eq!(pending.len(), 1, "the proposal must survive as a candidate");
        assert!(pending[0].title.contains("pnpm"));
        assert!(
            store.list_active().unwrap().is_empty(),
            "parking must never activate without consent (K36)"
        );
    }

    #[tokio::test]
    async fn approve_always_persists_rule_and_auto_allows_next_call() {
        let dir = tempfile::tempdir().unwrap();
        let approver = Arc::new(FixedApprover::new(ApprovalDecision::ApproveAlways));
        let executor = executor_for(dir.path(), approver.clone())
            .with_permission_rules_path(Some(leveler_execution::project_rules_path(dir.path())));
        let mut session = HashSet::new();

        executor
            .authorize(&rm_rf_call(), &mut session)
            .await
            .unwrap();
        assert_eq!(approver.asks(), 1);

        let set =
            leveler_execution::load_rules_file(&leveler_execution::project_rules_path(dir.path()))
                .unwrap();
        assert_eq!(set.rules().len(), 1);
        assert_eq!(
            set.rules()[0].match_.command_exact.as_deref(),
            Some("rm -rf scratch")
        );

        // A fresh session set is auto-allowed by the live rule set — the
        // approver is not asked again.
        let mut fresh_session = HashSet::new();
        executor
            .authorize(&rm_rf_call(), &mut fresh_session)
            .await
            .unwrap();
        assert_eq!(approver.asks(), 1);

        let mut changed = rm_rf_call();
        changed.arguments["args"] = serde_json::json!(["-rf", "other"]);
        executor
            .authorize(&changed, &mut fresh_session)
            .await
            .unwrap();
        assert_eq!(
            approver.asks(),
            2,
            "a different deletion target needs a fresh approval"
        );
    }

    #[tokio::test]
    async fn approve_session_stays_in_session_and_writes_no_grants_file() {
        let dir = tempfile::tempdir().unwrap();
        let approver = Arc::new(FixedApprover::new(ApprovalDecision::ApproveSession));
        let executor = executor_for(dir.path(), approver.clone());
        let mut session = HashSet::new();

        executor
            .authorize(&rm_rf_call(), &mut session)
            .await
            .unwrap();
        assert_eq!(approver.asks(), 1);
        assert!(
            !dir.path().join("permission_grants.json").exists(),
            "ApproveSession must not persist the legacy grants file"
        );

        // Same signature in-session: allowed without re-asking …
        executor
            .authorize(&rm_rf_call(), &mut session)
            .await
            .unwrap();
        assert_eq!(approver.asks(), 1);
        // … but a fresh session set asks again: nothing durable was recorded.
        let mut fresh_session = HashSet::new();
        executor
            .authorize(&rm_rf_call(), &mut fresh_session)
            .await
            .unwrap();
        assert_eq!(approver.asks(), 2);
    }

    #[tokio::test]
    async fn approve_always_without_rules_path_is_session_only() {
        let dir = tempfile::tempdir().unwrap();
        let approver = Arc::new(FixedApprover::new(ApprovalDecision::ApproveAlways));
        let executor = executor_for(dir.path(), approver.clone());
        let mut session = HashSet::new();

        executor
            .authorize(&rm_rf_call(), &mut session)
            .await
            .unwrap();
        assert!(
            !leveler_execution::project_rules_path(dir.path()).exists(),
            "no rules path configured → no rules file written"
        );

        executor
            .authorize(&rm_rf_call(), &mut session)
            .await
            .unwrap();
        assert_eq!(approver.asks(), 1, "session grant covers the repeat");
        let mut fresh_session = HashSet::new();
        executor
            .authorize(&rm_rf_call(), &mut fresh_session)
            .await
            .unwrap();
        assert_eq!(approver.asks(), 2, "nothing durable was recorded");
    }

    #[tokio::test]
    async fn approve_always_shell_script_persists_an_exact_rule() {
        // ApproveAlways on a compound shell now persists an EXACT rule (only
        // this verbatim command), not nothing — so it survives across sessions
        // without opening a `sh -c` prefix hole. A variant still asks.
        let dir = tempfile::tempdir().unwrap();
        let approver = Arc::new(FixedApprover::new(ApprovalDecision::ApproveAlways));
        let executor = executor_for(dir.path(), approver.clone())
            .with_permission_rules_path(Some(leveler_execution::project_rules_path(dir.path())));
        let mut session = HashSet::new();

        let script = ToolCall {
            id: ToolCallId::new("c"),
            name: "run_command".to_string(),
            arguments: serde_json::json!({"program": "sh", "args": ["-c", "rm -rf x"]}),
        };
        executor.authorize(&script, &mut session).await.unwrap();
        assert_eq!(approver.asks(), 1);

        let set =
            leveler_execution::load_rules_file(&leveler_execution::project_rules_path(dir.path()))
                .unwrap();
        assert_eq!(set.rules().len(), 1, "an exact rule must be persisted");
        assert_eq!(
            set.rules()[0].match_.command_prefix,
            None,
            "compound shell must never get a prefix rule"
        );
        assert!(
            set.rules()[0].match_.command_exact.is_some(),
            "it must be an exact-match rule"
        );

        // A fresh session is auto-allowed by the persisted exact rule.
        let mut fresh = HashSet::new();
        executor.authorize(&script, &mut fresh).await.unwrap();
        assert_eq!(
            approver.asks(),
            1,
            "exact rule covers the identical command"
        );

        // A DIFFERENT script still asks — exact, not prefix.
        let other = ToolCall {
            id: ToolCallId::new("c2"),
            name: "run_command".to_string(),
            arguments: serde_json::json!({"program": "sh", "args": ["-c", "rm -rf y"]}),
        };
        executor.authorize(&other, &mut fresh).await.unwrap();
        assert_eq!(approver.asks(), 2, "a variant must not ride the exact rule");
    }

    #[tokio::test]
    async fn memory_consent_does_not_persist_standing_or_session_grants() {
        let dir = tempfile::tempdir().unwrap();
        let approver = Arc::new(FixedApprover::new(ApprovalDecision::ApproveAlways));
        let executor = executor_for(dir.path(), approver.clone())
            .with_permission_rules_path(Some(leveler_execution::project_rules_path(dir.path())));
        let mut session = HashSet::new();

        let remember = ToolCall {
            id: ToolCallId::new("c"),
            name: "remember".to_string(),
            arguments: serde_json::json!({"title": "t", "content": "c"}),
        };
        executor.authorize(&remember, &mut session).await.unwrap();
        assert_eq!(approver.asks(), 1, "K36: memory writes always ask");
        executor.authorize(&remember, &mut session).await.unwrap();
        assert_eq!(approver.asks(), 2, "human consent remains exact-call only");
        assert!(session.is_empty());
        assert!(
            !leveler_execution::project_rules_path(dir.path()).exists(),
            "K36: memory writes never get standing permission"
        );
    }

    #[tokio::test]
    async fn write_path_glob_deny_rule_matches_call_paths() {
        let dir = tempfile::tempdir().unwrap();
        let approver = Arc::new(FixedApprover::new(ApprovalDecision::ApproveOnce));
        let deny_src = leveler_execution::PermissionRule {
            match_: leveler_execution::RuleMatch {
                tool: Some("apply_patch".into()),
                command_prefix: None,
                command_exact: None,
                write_path_glob: Some("src/**".into()),
            },
            effect: leveler_execution::RuleEffect::Deny,
        };
        let executor = executor_for(dir.path(), approver.clone()).with_permission_rules(
            leveler_execution::PermissionRuleSet::from_rules(vec![deny_src]),
        );
        let mut session = HashSet::new();

        let patch_src = ToolCall {
            id: ToolCallId::new("c"),
            name: "apply_patch".to_string(),
            arguments: serde_json::json!({
                "patch": "*** Begin Patch\n*** Update File: src/lib.rs\n@@\n-old\n+new\n*** End Patch"
            }),
        };
        let err = executor
            .authorize(&patch_src, &mut session)
            .await
            .unwrap_err();
        assert!(err.contains("permission rule"), "err: {err}");

        let patch_readme = ToolCall {
            id: ToolCallId::new("c"),
            name: "apply_patch".to_string(),
            arguments: serde_json::json!({
                "patch": "*** Begin Patch\n*** Update File: README.md\n@@\n-old\n+new\n*** End Patch"
            }),
        };
        executor
            .authorize(&patch_readme, &mut session)
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn approval_request_carries_command_and_scoped_paths() {
        let dir = tempfile::tempdir().unwrap();
        let approver = Arc::new(FixedApprover::new(ApprovalDecision::ApproveOnce));
        let executor = executor_for(dir.path(), approver.clone());
        let mut session = HashSet::new();

        let call = ToolCall {
            id: ToolCallId::new("c"),
            name: "run_command".to_string(),
            arguments: serde_json::json!({"program": "rm", "args": ["-rf", "x"], "cwd": "src"}),
        };
        executor.authorize(&call, &mut session).await.unwrap();
        let request = approver.last_request().unwrap();
        assert_eq!(request.command.as_deref(), Some("rm -rf x"));
        assert_eq!(request.paths, vec![std::path::PathBuf::from("src")]);
    }

    #[tokio::test]
    async fn a_read_only_command_is_not_called_destructive() {
        // `CommandClass::Dangerous` means "a human should look at this" — a
        // redirect outside the workspace trips it just as `rm` does. Rendering
        // that as "可能造成破坏性变更" tells the user something untrue about a
        // listing, and a risk line people learn to disbelieve is worse than no
        // risk line at all.
        let dir = tempfile::tempdir().unwrap();
        let approver = Arc::new(FixedApprover::new(ApprovalDecision::ApproveOnce));
        let executor = executor_for(dir.path(), approver.clone());
        let mut session = HashSet::new();

        let call = ToolCall {
            id: ToolCallId::new("c"),
            name: "shell_command".to_string(),
            arguments: serde_json::json!({"cmd": "ls -la src/ 2>/dev/null; git ls-files"}),
        };
        executor.authorize(&call, &mut session).await.unwrap();
        if let Some(req) = approver.last_request() {
            assert_ne!(req.risk, RiskLevel::Destructive, "cmd: {:?}", req.command);
        }
    }

    #[tokio::test]
    async fn a_destructive_shell_script_is_labelled_destructive() {
        let dir = tempfile::tempdir().unwrap();
        let approver = Arc::new(FixedApprover::new(ApprovalDecision::ApproveOnce));
        let executor = executor_for(dir.path(), approver.clone());
        let mut session = HashSet::new();

        let call = ToolCall {
            id: ToolCallId::new("c"),
            name: "shell_command".to_string(),
            arguments: serde_json::json!({"cmd": "rm -rf src/main.rs && ls src/"}),
        };
        executor.authorize(&call, &mut session).await.unwrap();
        assert_eq!(
            approver.last_request().unwrap().risk,
            RiskLevel::Destructive
        );
    }

    #[tokio::test]
    async fn a_destructive_command_is_labelled_destructive() {
        // The policy already classifies `rm -rf` as dangerous to decide that it
        // needs asking. The prompt the user reads must say so too, otherwise a
        // file deletion looks exactly as harmless as `ls`.
        let dir = tempfile::tempdir().unwrap();
        let approver = Arc::new(FixedApprover::new(ApprovalDecision::ApproveOnce));
        let executor = executor_for(dir.path(), approver.clone());
        let mut session = HashSet::new();

        let call = ToolCall {
            id: ToolCallId::new("c"),
            name: "run_command".to_string(),
            arguments: serde_json::json!({"program": "rm", "args": ["-rf", "x"]}),
        };
        executor.authorize(&call, &mut session).await.unwrap();
        assert_eq!(
            approver.last_request().unwrap().risk,
            RiskLevel::Destructive
        );
    }

    #[tokio::test]
    async fn a_harmless_command_is_not_labelled_destructive() {
        let dir = tempfile::tempdir().unwrap();
        let approver = Arc::new(FixedApprover::new(ApprovalDecision::ApproveOnce));
        let executor = executor_for(dir.path(), approver.clone());
        let mut session = HashSet::new();

        let call = ToolCall {
            id: ToolCallId::new("c"),
            name: "run_command".to_string(),
            arguments: serde_json::json!({"program": "ls", "args": ["-l"]}),
        };
        executor.authorize(&call, &mut session).await.unwrap();
        // `ls` may be auto-allowed outright; either way it must never be
        // labelled destructive.
        if let Some(req) = approver.last_request() {
            assert_ne!(req.risk, RiskLevel::Destructive);
        }
    }

    #[tokio::test]
    async fn the_prompt_summary_is_not_english_filler() {
        // "shell_command requested by the model" tells the user nothing the
        // tool row above it did not already say, in the wrong language.
        let dir = tempfile::tempdir().unwrap();
        let approver = Arc::new(FixedApprover::new(ApprovalDecision::ApproveOnce));
        let executor = executor_for(dir.path(), approver.clone());
        let mut session = HashSet::new();

        let call = ToolCall {
            id: ToolCallId::new("c"),
            name: "run_command".to_string(),
            arguments: serde_json::json!({"program": "rm", "args": ["-rf", "x"]}),
        };
        executor.authorize(&call, &mut session).await.unwrap();
        let summary = approver.last_request().unwrap().description;
        assert!(
            !summary.contains("requested by the model"),
            "description is filler: {summary}"
        );
    }

    #[tokio::test]
    async fn full_product_contract_bypasses_every_representative_permission_gate() {
        use leveler_execution::{
            NetworkScope, PermissionRule, PermissionRuleSet, RuleEffect, RuleMatch,
        };
        let dir = tempfile::tempdir().unwrap();
        let approver = Arc::new(FixedApprover::new(ApprovalDecision::Deny));
        let mut exec = executor_at(dir.path(), PermissionProfile::FullAccess, approver.clone());
        exec.tool_context = exec.tool_context.clone().with_sandbox(true);
        exec.tool_context.policy.read_only = true;
        exec.tool_context.policy.command_write_allowlist = Some(Arc::new(Vec::new()));
        *exec.permission_rules.write().unwrap() = PermissionRuleSet::from_rules(
            [
                "run_command",
                "write_file",
                "read_file",
                "remember",
                "save_agent",
            ]
            .iter()
            .map(|name| PermissionRule {
                match_: RuleMatch {
                    tool: Some((*name).into()),
                    ..Default::default()
                },
                effect: RuleEffect::Deny,
            })
            .collect(),
        );
        let actions = [
            call(
                "write_file",
                serde_json::json!({"path":"ordinary.txt","content":"x"}),
            ),
            call("read_file", serde_json::json!({"path":"/tmp/.env"})),
            call(
                "run_command",
                serde_json::json!({"program":"git","args":["fetch","https://unknown.example/repo"]}),
            ),
            call(
                "run_command",
                serde_json::json!({"program":"git","args":["push","--force","origin","HEAD"]}),
            ),
            call(
                "run_command",
                serde_json::json!({"program":"curl","args":["https://example.com"]}),
            ),
            call(
                "run_command",
                serde_json::json!({"program":"curl","args":["http://localhost:3000"]}),
            ),
            call(
                "run_command",
                serde_json::json!({"program":"kill","args":["12345"]}),
            ),
            call(
                "run_command",
                serde_json::json!({"program":"git","args":["credential","fill"]}),
            ),
            call("remember", serde_json::json!({"title":"x","body":"y"})),
            call("save_agent", serde_json::json!({})),
        ];
        for action in actions {
            let admitted = exec
                .admit(
                    action.clone(),
                    exec.tool_context.clone(),
                    false,
                    &mut HashSet::new(),
                    &CancellationToken::new(),
                )
                .await;
            let admitted = match admitted {
                Ok(admitted) => admitted,
                Err(AdmitError::Refused { reason, .. }) => panic!("{}: {reason}", action.name),
                Err(AdmitError::Fatal(error)) => panic!("{error}"),
            };
            assert!(admitted.resolved().unrestricted_execution());
            assert_eq!(admitted.resolved().write, WriteScope::Unrestricted);
            assert_eq!(admitted.resolved().network_scope, NetworkScope::Internet);
        }
        assert_eq!(approver.asks(), 0);
    }

    #[tokio::test]
    async fn auto_product_contract_allows_development_and_asks_for_danger() {
        let dir = tempfile::tempdir().unwrap();
        let approver = Arc::new(FixedApprover::new(ApprovalDecision::ApproveOnce));
        let exec = executor_at(dir.path(), PermissionProfile::Assisted, approver.clone());
        for args in [
            vec!["status"],
            vec!["fetch", "origin"],
            vec!["commit", "-m", "normal"],
        ] {
            let action = call(
                "run_command",
                serde_json::json!({"program":"git","args":args}),
            );
            assert!(
                matches!(resolve(&exec, &action).await, PolicyResolution::Allow(_)),
                "{args:?}"
            );
        }
        for args in [
            vec!["push", "origin", "HEAD"],
            vec!["reset", "--hard"],
            vec!["clean", "-fd"],
        ] {
            let action = call(
                "run_command",
                serde_json::json!({"program":"git","args":args}),
            );
            assert!(
                matches!(resolve(&exec, &action).await, PolicyResolution::Ask(_)),
                "{args:?}"
            );
        }
        assert_eq!(approver.asks(), 0, "resolution does not invoke approval");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn auto_approved_exact_call_executes_outside_workspace_without_second_refusal() {
        let dir = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let victim = outside.path().join("approved-target");
        std::fs::write(&victim, "fixture").unwrap();
        let approver = Arc::new(FixedApprover::new(ApprovalDecision::ApproveOnce));
        let mut exec = executor_at(dir.path(), PermissionProfile::Assisted, approver.clone());
        exec.tool_context = exec.tool_context.clone().with_sandbox(true);
        let action = call(
            "run_command",
            serde_json::json!({"program":"rm","args":["-rf",victim]}),
        );
        let admitted = match exec
            .admit(
                action,
                exec.tool_context.clone(),
                false,
                &mut HashSet::new(),
                &CancellationToken::new(),
            )
            .await
        {
            Ok(admitted) => admitted,
            Err(_) => panic!("approved exact call must be admitted"),
        };
        assert_eq!(approver.asks(), 1);
        assert!(admitted.resolved().unrestricted_execution());
        let (content, failed, _) = exec
            .dispatch_raw(&admitted, &CancellationToken::new(), None)
            .await;
        assert!(!failed, "{content}");
        assert!(
            !victim.exists(),
            "observable approved effect must actually occur"
        );
        drop(admitted);
        let next = call("run_command", serde_json::json!({"program":"ls","args":[]}));
        match resolve(&exec, &next).await {
            PolicyResolution::Allow(policy) => assert!(!policy.unrestricted_execution()),
            other => panic!("{other:?}"),
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn auto_foreign_task_asks_once_and_approved_stop_executes() {
        let dir = tempfile::tempdir().unwrap();
        let tasks = Arc::new(leveler_execution::BackgroundTaskRegistry::new());
        let request = leveler_execution::ProcessRequest::new(
            "sleep",
            vec!["30".into()],
            dir.path().to_path_buf(),
        );
        let id = tasks
            .spawn_owned(request, None, Some("creator"))
            .await
            .unwrap();
        let mut registry = default_registry();
        registry.register(Arc::new(leveler_tools::tools::KillTaskTool::new(
            tasks.clone(),
        )));
        let approver = Arc::new(FixedApprover::new(ApprovalDecision::ApproveOnce));
        let mut exec = executor_at(dir.path(), PermissionProfile::Assisted, approver.clone());
        exec.registry = Arc::new(registry);
        exec.tool_context = exec.tool_context.clone().with_session_scope("creator");
        let action = call("kill_task", serde_json::json!({"task_id":id}));
        assert!(matches!(
            resolve(&exec, &action).await,
            PolicyResolution::Allow(_)
        ));
        exec.tool_context = exec
            .tool_context
            .clone()
            .with_session_scope("another-session");
        assert!(matches!(
            resolve(&exec, &action).await,
            PolicyResolution::Ask(_)
        ));
        let admitted = match exec
            .admit(
                action,
                exec.tool_context.clone(),
                false,
                &mut HashSet::new(),
                &CancellationToken::new(),
            )
            .await
        {
            Ok(admitted) => admitted,
            Err(_) => panic!("approved foreign task control must be admitted"),
        };
        let (content, failed, _) = exec
            .dispatch_raw(&admitted, &CancellationToken::new(), None)
            .await;
        assert!(!failed, "{content}");
        assert_eq!(approver.asks(), 1);
        let snapshot = tasks.get(&id).await.unwrap();
        assert_eq!(
            snapshot.status,
            leveler_execution::BackgroundTaskStatus::Killed
        );
        assert_eq!(snapshot.owner_scope.as_deref(), Some("creator"));
    }

    fn resource_repo(root: &std::path::Path) {
        for args in [
            vec!["init", "-q"],
            vec!["config", "credential.helper", ""],
            vec!["remote", "add", "origin", "https://example.com/a.git"],
        ] {
            assert!(
                std::process::Command::new("git")
                    .current_dir(root)
                    .args(args)
                    .output()
                    .unwrap()
                    .status
                    .success()
            );
        }
    }

    fn resource_executor(
        dir: &std::path::Path,
        mode: PermissionProfile,
        network_enforceable: bool,
    ) -> Executor {
        let mut executor = net_executor(dir, mode, network_enforceable);
        executor.tool_context = ToolContext::with_environment(
            Workspace::new(dir).unwrap(),
            mode,
            Arc::new(leveler_core::environment().clone()),
        );
        executor
    }

    async fn admit_resource(exec: &Executor, action: ToolCall) -> AdmittedCall {
        match exec
            .admit(
                action,
                exec.tool_context.clone(),
                false,
                &mut HashSet::new(),
                &CancellationToken::new(),
            )
            .await
        {
            Ok(admitted) => admitted,
            Err(AdmitError::Refused { reason, .. }) => panic!("{reason}"),
            Err(AdmitError::Fatal(error)) => panic!("{error}"),
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn resource_store_restart_reuses_only_the_approved_git_target() {
        use leveler_storage::ResourceGrantStore;
        let dir = tempfile::tempdir().unwrap();
        resource_repo(dir.path());
        let path = dir.path().join("grants.db");
        let request = leveler_execution::resolve_git_grant(
            "git",
            &["fetch".into(), "origin".into()],
            dir.path(),
        )
        .await
        .unwrap()
        .unwrap();
        let store = Arc::new(leveler_storage::Database::connect(&path).await.unwrap());
        store
            .grant(
                &request.project_identity,
                "real-session-A",
                leveler_core::GrantScope::Session,
                &request.bindings,
            )
            .await
            .unwrap();
        drop(store);
        let reopened = Arc::new(leveler_storage::Database::connect(&path).await.unwrap());
        assert!(
            reopened
                .covers(
                    &request.project_identity,
                    "real-session-A",
                    &request.bindings
                )
                .await
                .unwrap()
        );
        assert!(
            !reopened
                .covers(
                    &request.project_identity,
                    "real-session-B",
                    &request.bindings
                )
                .await
                .unwrap()
        );
        let mut resumed = resource_executor(dir.path(), PermissionProfile::RequestApproval, false)
            .with_resource_grants(reopened);
        resumed.tool_context = resumed
            .tool_context
            .clone()
            .with_session_scope("real-session-A");
        let fetch = call(
            "run_command",
            serde_json::json!({"program":"git","args":["fetch","origin"]}),
        );
        // The stored exact grant covers the same repository and remote.
        match resolve(&resumed, &fetch).await {
            PolicyResolution::Allow(policy) => assert!(
                policy.resource_grant().is_some(),
                "the stored exact Git grant must authorize the same call"
            ),
            other => panic!("the stored exact Git grant must authorize the same call: {other:?}"),
        }
        // A changed remote is a different resource: the old grant must not cover
        // it, and the new target is offered for a fresh decision.
        assert!(
            std::process::Command::new("git")
                .current_dir(dir.path())
                .args(["remote", "set-url", "origin", "https://example.com/b.git"])
                .status()
                .unwrap()
                .success()
        );
        let changed = leveler_execution::resolve_git_grant(
            "git",
            &["fetch".into(), "origin".into()],
            dir.path(),
        )
        .await
        .unwrap()
        .unwrap();
        assert_ne!(request, changed);
        match resolve(&resumed, &fetch).await {
            PolicyResolution::Ask(pending) => {
                let offered = pending
                    .request
                    .grant
                    .as_ref()
                    .expect("the changed target is offered for a fresh decision");
                assert_eq!(offered, &changed);
                assert_ne!(offered, &request);
            }
            other => panic!("a changed Git remote must require fresh consent: {other:?}"),
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn resource_shell_wrapped_git_keeps_exact_call_approval() {
        let dir = tempfile::tempdir().unwrap();
        resource_repo(dir.path());
        let nested = dir.path().join("nested");
        std::fs::create_dir(&nested).unwrap();
        resource_repo(&nested);
        let exec = resource_executor(dir.path(), PermissionProfile::Assisted, true);
        // A shell command is opaque: it cannot consume a frozen Git target, so it
        // never offers one, whatever workdir it names.
        for arguments in [
            serde_json::json!({"cmd":"git push origin main","workdir":"nested"}),
            serde_json::json!({"cmd":"git push origin main"}),
        ] {
            let action = call("shell_command", arguments);
            assert!(
                exec.registry
                    .get("shell_command")
                    .unwrap()
                    .command_grant_request(&action.arguments, &exec.tool_context)
                    .await
                    .unwrap()
                    .is_none(),
                "a shell-wrapped Git command must keep exact-call approval"
            );
        }
        // The argv tool still resolves its target against the directory it runs in.
        let action = call(
            "run_command",
            serde_json::json!({"program":"git","args":["fetch","origin"],"cwd":"nested"}),
        );
        let request = exec
            .registry
            .get("run_command")
            .unwrap()
            .command_grant_request(&action.arguments, &exec.tool_context)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            request.project_identity,
            leveler_execution::resolve_project_identity(&nested)
                .await
                .unwrap()
        );
    }

    #[tokio::test]
    async fn resource_unproven_filesystem_consumers_do_not_offer_reusable_authority() {
        let dir = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let target = outside.path().join("target");
        std::fs::write(&target, "before").unwrap();
        let exec = resource_executor(dir.path(), PermissionProfile::Assisted, true);
        let action = call(
            "write_file",
            serde_json::json!({"path":target,"content":"after"}),
        );
        match resolve(&exec, &action).await {
            PolicyResolution::Ask(pending) => assert!(
                pending.request.grant.is_none(),
                "path checks alone cannot authorize the subsequently opened/replaced object"
            ),
            other => panic!("expected approval, got {other:?}"),
        }
    }

    /// Unix only: a reusable Git grant needs the object-incarnation proof the
    /// execution layer can only produce there; Windows keeps exact-call approval.
    #[cfg(unix)]
    #[tokio::test]
    async fn resource_git_consumers_offer_only_the_approved_remote() {
        let dir = tempfile::tempdir().unwrap();
        resource_repo(dir.path());
        let exec = resource_executor(dir.path(), PermissionProfile::Assisted, true);
        for args in [
            vec!["push", "origin", "main"],
            vec!["push", "--force", "origin", "main"],
        ] {
            let action = call(
                "run_command",
                serde_json::json!({"program":"git","args":args}),
            );
            match resolve(&exec, &action).await {
                PolicyResolution::Ask(pending) => {
                    let grant = pending
                        .request
                        .grant
                        .as_ref()
                        .expect("a dangerous Git remote action offers its approved target");
                    assert!(grant.bindings.iter().any(|binding| matches!(
                        &binding.resource,
                        leveler_core::ResourceIdentity::ConfiguredRemote { .. }
                    )));
                    assert!(
                        pending
                            .request
                            .decisions()
                            .contains(&ApprovalDecision::ApproveProject)
                    );
                }
                other => panic!("a dangerous Git push must ask: {other:?}"),
            }
        }
    }

    struct NetworkTool;

    #[async_trait::async_trait]
    impl leveler_tools::Tool for NetworkTool {
        fn name(&self) -> &str {
            "net_probe"
        }
        fn description(&self) -> &str {
            "reaches the network (test-only)"
        }
        fn input_schema(&self) -> serde_json::Value {
            serde_json::json!({ "type": "object", "additionalProperties": true })
        }
        fn risk(&self) -> RiskLevel {
            RiskLevel::Network
        }
        async fn execute(
            &self,
            _input: serde_json::Value,
            _context: ToolContext,
            _cancellation: CancellationToken,
        ) -> Result<leveler_tools::ToolOutput, leveler_tools::ToolError> {
            Ok(leveler_tools::ToolOutput::ok("ok"))
        }
    }

    fn net_executor(
        dir: &std::path::Path,
        mode: PermissionProfile,
        network_enforceable: bool,
    ) -> Executor {
        let mut registry = default_registry();
        registry.register(Arc::new(NetworkTool));
        registry.register(Arc::new(leveler_tools::tools::WebFetchTool));
        Executor::new(
            Arc::new(StubRuntime),
            Arc::new(registry),
            ToolContext::new(Workspace::new(dir).unwrap(), mode),
            ModelRef::new("mock", "m"),
            10,
        )
        .with_approver(Arc::new(FixedApprover::new(ApprovalDecision::Deny)))
        .with_network_enforceable(network_enforceable)
    }

    fn call(name: &str, arguments: serde_json::Value) -> ToolCall {
        ToolCall {
            id: ToolCallId::new("n"),
            name: name.to_string(),
            arguments,
        }
    }

    async fn resolve(exec: &Executor, call: &ToolCall) -> leveler_execution::PolicyResolution {
        exec.resolve_policy(call, &exec.tool_context, &CancellationToken::new())
            .await
    }

    /// 请求批准: an ordinary workspace command runs without a prompt, and runs
    /// with the network denied. A command that needs the network — curl, a
    /// Python script, a build.rs — is stopped by the sandbox, not guessed from
    /// its name.
    #[tokio::test]
    async fn request_approval_runs_ordinary_commands_without_network() {
        use leveler_execution::PolicyResolution;
        let dir = tempfile::tempdir().unwrap();
        let exec = net_executor(dir.path(), PermissionProfile::RequestApproval, true);
        for arguments in [
            serde_json::json!({"program": "cargo", "args": ["test"]}),
            serde_json::json!({"program": "curl", "args": ["-sI", "https://example.com"]}),
            serde_json::json!({"program": "python3", "args": ["fetch.py"]}),
        ] {
            match resolve(&exec, &call("run_command", arguments.clone())).await {
                PolicyResolution::Allow(resolved) => {
                    assert_eq!(
                        resolved.network_scope,
                        leveler_execution::NetworkScope::Loopback,
                        "{arguments}"
                    )
                }
                other => panic!("{arguments}: {other:?}"),
            }
        }
    }

    /// 完全访问 and 替我审批 keep reaching the network directly.
    #[tokio::test]
    async fn full_access_and_assisted_commands_reach_the_network() {
        use leveler_execution::PolicyResolution;
        let dir = tempfile::tempdir().unwrap();
        for mode in [PermissionProfile::FullAccess, PermissionProfile::Assisted] {
            let exec = net_executor(dir.path(), mode, true);
            let curl = call(
                "run_command",
                serde_json::json!({"program": "curl", "args": ["-sI", "https://example.com"]}),
            );
            match resolve(&exec, &curl).await {
                PolicyResolution::Allow(resolved) => assert_eq!(
                    resolved.network_scope,
                    leveler_execution::NetworkScope::Internet,
                    "{mode:?}"
                ),
                other => panic!("{mode:?}: {other:?}"),
            }
        }
    }

    /// Approving a tool whose need IS the network grants the network for that
    /// call — one prompt, not an approval followed by a sandbox refusal.
    #[tokio::test]
    async fn approving_a_network_tool_grants_that_call_the_network() {
        use leveler_execution::PolicyResolution;
        let dir = tempfile::tempdir().unwrap();
        let exec = net_executor(dir.path(), PermissionProfile::RequestApproval, true);
        match resolve(&exec, &call("net_probe", serde_json::json!({}))).await {
            PolicyResolution::Ask(pending) => assert_eq!(
                pending.network_scope,
                leveler_execution::NetworkScope::Internet
            ),
            other => panic!("{other:?}"),
        }
        // An MCP server is outside the sandbox: under the profile default it
        // is asked like any network use...
        match resolve(&exec, &call("mcp__srv__fetch", serde_json::json!({}))).await {
            PolicyResolution::Ask(pending) => assert_eq!(
                pending.network_scope,
                leveler_execution::NetworkScope::Internet
            ),
            other => panic!("{other:?}"),
        }
        // ...and refused outright when the run itself denies the network.
        let mut exec = net_executor(dir.path(), PermissionProfile::RequestApproval, true);
        exec.tool_context = exec.tool_context.clone().with_sandbox(true);
        assert!(matches!(
            resolve(&exec, &call("mcp__srv__fetch", serde_json::json!({}))).await,
            PolicyResolution::Deny(_)
        ));
    }

    /// A host that cannot deny a command the network (Windows) must not pretend
    /// to: under 请求批准 every command is put to the user first, the prompt
    /// says the network cannot be blocked, and an approval lets it run open.
    #[tokio::test]
    async fn without_network_enforcement_request_approval_asks_before_every_command() {
        use leveler_execution::PolicyResolution;
        let dir = tempfile::tempdir().unwrap();
        let exec = net_executor(dir.path(), PermissionProfile::RequestApproval, false);
        match resolve(
            &exec,
            &call(
                "run_command",
                serde_json::json!({"program": "cargo", "args": ["test"]}),
            ),
        )
        .await
        {
            PolicyResolution::Ask(pending) => {
                assert_eq!(
                    pending.network_scope,
                    leveler_execution::NetworkScope::Internet
                );
                assert!(
                    pending.request.description.contains("无法断网"),
                    "{}",
                    pending.request.description
                );
            }
            other => panic!("{other:?}"),
        }
        // Assisted never denied the network, so nothing changes there.
        let exec = net_executor(dir.path(), PermissionProfile::Assisted, false);
        assert!(matches!(
            resolve(
                &exec,
                &call(
                    "run_command",
                    serde_json::json!({"program": "cargo", "args": ["test"]})
                ),
            )
            .await,
            PolicyResolution::Allow(_)
        ));
    }

    // ---- PR 5: authorization is decided in one place and frozen per call ----

    /// Resolution is pure: hooks → rules → profile policy decide Allow / Ask
    /// / Deny without touching the approver. Asking is a separate step.
    #[tokio::test]
    async fn policy_resolution_is_decided_before_anyone_is_asked() {
        use leveler_execution::{AuthorizationEvidence, PolicyResolution, WriteScope};
        let dir = tempfile::tempdir().unwrap();
        let approver = Arc::new(FixedApprover::new(ApprovalDecision::Deny));
        let exec = executor_for(dir.path(), approver.clone());
        let ctx = exec.tool_context.clone();
        let root = ctx.require_workspace().unwrap().root().to_path_buf();

        match exec
            .resolve_policy(&grep_call(), &ctx, &CancellationToken::new())
            .await
        {
            PolicyResolution::Allow(resolved) => {
                assert!(
                    matches!(resolved.authorization, AuthorizationEvidence::Policy { .. }),
                    "{:?}",
                    resolved.authorization
                );
                assert_eq!(resolved.write, WriteScope::Workspace { root: root.clone() });
                assert_eq!(
                    resolved.network_scope,
                    leveler_execution::NetworkScope::Internet
                );
            }
            other => panic!("grep under assisted must resolve to Allow: {other:?}"),
        }
        assert!(matches!(
            exec.resolve_policy(&rm_rf_call(), &ctx, &CancellationToken::new())
                .await,
            PolicyResolution::Ask(_)
        ));
        assert_eq!(approver.asks(), 0, "resolution must not ask anyone");
    }

    /// The admitted call carries its policy. A profile switch after admission
    /// reaches the NEXT call, never this one — and the context the tool
    /// executes with is that frozen policy, not the live cell.
    #[tokio::test]
    async fn an_admitted_call_carries_a_frozen_policy() {
        use leveler_execution::WriteScope;
        let dir = tempfile::tempdir().unwrap();
        let profile = leveler_execution::SharedPermissionProfile::new(PermissionProfile::Assisted);
        let approver = Arc::new(FixedApprover::new(ApprovalDecision::ApproveOnce));
        let exec = executor_sharing(dir.path(), &profile, approver);
        let ctx = exec.tool_context.clone();
        let root = ctx.require_workspace().unwrap().root().to_path_buf();
        let admitted = exec
            .admit(
                grep_call(),
                ctx,
                false,
                &mut HashSet::new(),
                &CancellationToken::new(),
            )
            .await
            .ok()
            .expect("grep is admitted under assisted");
        let confined = WriteScope::Workspace { root: root.clone() };
        assert_eq!(admitted.resolved().write, confined);

        profile.set(PermissionProfile::FullAccess);
        assert_eq!(
            admitted.resolved().write,
            confined,
            "a switch after admission must not widen an admitted call"
        );
        assert_eq!(
            admitted.execution_context().write_scope(),
            confined,
            "the tool executes under the frozen policy, not the live profile"
        );
    }

    /// Session approval executes that exact action without a second scope gate.
    #[tokio::test]
    async fn approve_session_executes_the_exact_action_without_another_permission_gate() {
        use leveler_execution::{AuthorizationEvidence, WriteScope};
        let dir = tempfile::tempdir().unwrap();
        let approver = Arc::new(FixedApprover::new(ApprovalDecision::ApproveSession));
        let exec = executor_for(dir.path(), approver.clone());
        let ctx = exec.tool_context.clone();
        let root = ctx.require_workspace().unwrap().root().to_path_buf();
        let mut session = HashSet::new();
        let first = exec
            .admit(
                rm_rf_call(),
                ctx.clone(),
                false,
                &mut session,
                &CancellationToken::new(),
            )
            .await
            .ok()
            .expect("first admitted after approval");
        assert_eq!(
            first.resolved().write,
            WriteScope::Unrestricted,
            "explicit approval must not be rejected by another scope gate"
        );
        // A command admission holds the execution gate until the call is
        // released, so the first must be dropped before the second can be
        // admitted (the drive dispatches each serial call before admitting the
        // next; this test drives admission directly).
        drop(first);
        let second = exec
            .admit(
                rm_rf_call(),
                ctx,
                false,
                &mut session,
                &CancellationToken::new(),
            )
            .await
            .ok()
            .expect("second admitted on the session grant");
        assert_eq!(approver.asks(), 1, "the second call must not re-prompt");
        assert_eq!(
            second.resolved().write,
            WriteScope::Unrestricted,
            "explicit approval must not be rejected by another scope gate"
        );
        assert!(matches!(
            second.resolved().authorization,
            AuthorizationEvidence::SessionGrant { .. }
        ));
        match resolve(&exec, &grep_call()).await {
            PolicyResolution::Allow(policy) => {
                assert_eq!(policy.write, WriteScope::Workspace { root })
            }
            other => panic!("{other:?}"),
        }
    }

    /// `request_permissions` goes through the host's ask path. An allowing
    /// auto-reviewer settles it without reaching the human approver — the
    /// direct-to-approver bypass the migration plan pointed at.
    #[tokio::test]
    async fn request_permissions_is_settled_by_the_same_ask_path_as_tool_calls() {
        struct AllowAll;
        #[async_trait::async_trait]
        impl leveler_execution::AutoReviewer for AllowAll {
            async fn review(&self, _request: &ApprovalRequest) -> ReviewVerdict {
                ReviewVerdict::Allow
            }
        }
        let dir = tempfile::tempdir().unwrap();
        let approver = Arc::new(FixedApprover::new(ApprovalDecision::Deny));
        let exec =
            executor_for(dir.path(), approver.clone()).with_auto_reviewer(Arc::new(AllowAll));
        let call = ToolCall {
            id: ToolCallId::new("p"),
            name: "request_permissions".to_string(),
            arguments: serde_json::json!({"action": "fetch deps", "network": true}),
        };
        let outcome = exec
            .handle_request_permissions(&call, &CancellationToken::new())
            .await
            .unwrap();
        assert!(
            matches!(
                outcome,
                crate::injected_tools::PermissionRequestOutcome::Granted { .. }
            ),
            "{outcome:?}"
        );
        assert_eq!(
            approver.asks(),
            0,
            "the reviewer settled it; the human was never asked"
        );
    }
    #[derive(Default)]
    struct PoisonedResourceStore {
        covers: std::sync::atomic::AtomicUsize,
        writes: std::sync::atomic::AtomicUsize,
    }

    #[async_trait::async_trait]
    impl leveler_storage::ResourceGrantStore for PoisonedResourceStore {
        async fn covers(
            &self,
            _: &str,
            _: &str,
            _: &[leveler_core::GrantBinding],
        ) -> Result<bool, leveler_storage::StorageError> {
            self.covers
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Err(leveler_storage::StorageError::InvalidData(
                "synthetic corrupted grant store".into(),
            ))
        }
        async fn grant(
            &self,
            _: &str,
            _: &str,
            _: leveler_core::GrantScope,
            _: &[leveler_core::GrantBinding],
        ) -> Result<(), leveler_storage::StorageError> {
            self.writes
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Err(leveler_storage::StorageError::InvalidData(
                "synthetic unavailable grant store".into(),
            ))
        }
    }

    struct FullGrantProbe;
    #[async_trait::async_trait]
    impl leveler_tools::Tool for FullGrantProbe {
        fn name(&self) -> &'static str {
            "full_grant_probe"
        }
        fn description(&self) -> &'static str {
            "Test Full bypass across the resource capability matrix."
        }
        fn input_schema(&self) -> serde_json::Value {
            serde_json::json!({"type":"object"})
        }
        fn risk(&self) -> RiskLevel {
            RiskLevel::Privileged
        }
        fn approval_reason(&self, _: &serde_json::Value, _: &ToolContext) -> Option<String> {
            panic!("Full must bypass resource approval preflight")
        }
        async fn grant_bindings(
            &self,
            _: &serde_json::Value,
            _: &ToolContext,
        ) -> Result<Option<Vec<leveler_core::GrantBinding>>, String> {
            panic!("Full must not resolve reusable resource identities")
        }
        async fn execute(
            &self,
            _: serde_json::Value,
            _: ToolContext,
            _: CancellationToken,
        ) -> Result<leveler_tools::ToolOutput, leveler_tools::ToolError> {
            unreachable!("this matrix tests admission, real Full effects have separate coverage")
        }
    }

    #[tokio::test]
    async fn resource_full_capability_matrix_bypasses_poisoned_store_and_approval() {
        use leveler_core::Capability::*;
        use std::sync::atomic::Ordering;
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(PoisonedResourceStore::default());
        let approver = Arc::new(FixedApprover::new(ApprovalDecision::Deny));
        let mut executor = resource_executor(dir.path(), PermissionProfile::FullAccess, true)
            .with_resource_grants(store.clone())
            .with_approver(approver.clone());
        let mut registry = default_registry();
        registry.register(Arc::new(FullGrantProbe));
        executor.registry = Arc::new(registry);
        for capability in [
            RepositoryRead,
            RepositoryMutate,
            RepositoryMetadataWrite,
            RepositoryConfigWrite,
            RepositoryDestroy,
            RemoteRead,
            RemoteMutate,
            RemoteForce,
            FilesystemRead,
            FilesystemWrite,
            FilesystemDelete,
            BackgroundTaskObserve,
            BackgroundTaskControl,
            ExternalProcessControl,
            CredentialUse,
            CredentialRawRead,
            CredentialRawWrite,
        ] {
            let action = call(
                "full_grant_probe",
                serde_json::json!({"capability":capability}),
            );
            let admitted = admit_resource(&executor, action).await;
            assert!(
                admitted.resolved().unrestricted_execution(),
                "{capability:?}"
            );
        }
        assert_eq!(store.covers.load(Ordering::SeqCst), 0);
        assert_eq!(store.writes.load(Ordering::SeqCst), 0);
        assert_eq!(approver.asks(), 0);
    }

    #[tokio::test]
    async fn resource_auto_ordinary_write_ignores_poisoned_store_and_dangerous_action_still_asks() {
        use std::sync::atomic::Ordering;
        let dir = tempfile::tempdir().unwrap();
        resource_repo(dir.path());
        let store = Arc::new(PoisonedResourceStore::default());
        let approver = Arc::new(FixedApprover::new(ApprovalDecision::Deny));
        let executor = resource_executor(dir.path(), PermissionProfile::Assisted, true)
            .with_resource_grants(store.clone())
            .with_approver(approver.clone());
        let ordinary = call(
            "write_file",
            serde_json::json!({"path":"ordinary.txt","content":"actual-auto-write"}),
        );
        let admitted = admit_resource(&executor, ordinary).await;
        let (content, failed, _) = executor
            .dispatch_raw(&admitted, &CancellationToken::new(), None)
            .await;
        assert!(!failed, "{content}");
        assert_eq!(
            std::fs::read_to_string(dir.path().join("ordinary.txt")).unwrap(),
            "actual-auto-write"
        );
        assert_eq!(store.covers.load(Ordering::SeqCst), 0);
        assert_eq!(store.writes.load(Ordering::SeqCst), 0);
        assert_eq!(approver.asks(), 0);
        drop(admitted);
        let dangerous = call(
            "run_command",
            serde_json::json!({"program":"git","args":["push","origin","main"]}),
        );
        let result = executor
            .admit(
                dangerous,
                executor.tool_context.clone(),
                false,
                &mut HashSet::new(),
                &CancellationToken::new(),
            )
            .await;
        assert!(matches!(result, Err(AdmitError::Refused { .. })));
        assert_eq!(approver.asks(), 1);
        assert_eq!(store.writes.load(Ordering::SeqCst), 0);
    }

    /// Phase 4: a credential binding is reusable only because the frozen Git
    /// target re-verifies the credential incarnation at execution. On its own
    /// it authorizes nothing — a credential is never a remote-effect grant.
    /// Unix only: `execution_bound_request` refuses a frozen Git target on a
    /// platform without object-incarnation proof.
    #[cfg(unix)]
    #[test]
    fn a_credential_binding_is_execution_bound_but_never_authorizes_alone() {
        use leveler_core::{Capability, GrantBinding, GrantRequest, ResourceIdentity};
        let remote = GrantBinding {
            capability: Capability::RemoteRead,
            resource: ResourceIdentity::ConfiguredRemote {
                repository: "repo-epoch".into(),
                remote_name: "origin".into(),
                canonical_url: "https://example.com/private.git".into(),
                transport: "https".into(),
            },
        };
        let credential = GrantBinding {
            capability: Capability::CredentialUse,
            resource: ResourceIdentity::Credential {
                project: "repo-epoch".into(),
                host: "example.com".into(),
                identity: "sha256:0123456789abcdef".into(),
                transport: "https".into(),
            },
        };
        let with_remote = GrantRequest {
            project_identity: "repo-epoch".into(),
            bindings: vec![remote.clone(), credential.clone()],
        };
        assert!(
            Executor::execution_bound_request(&with_remote),
            "the frozen Git target is the credential binding's execution owner"
        );
        let credential_only = GrantRequest {
            project_identity: "repo-epoch".into(),
            bindings: vec![credential],
        };
        assert!(
            !Executor::execution_bound_request(&credential_only),
            "a credential alone must not authorize a remote effect"
        );
        let raw_read = GrantRequest {
            project_identity: "repo-epoch".into(),
            bindings: vec![
                remote,
                GrantBinding {
                    capability: Capability::CredentialRawRead,
                    resource: ResourceIdentity::Credential {
                        project: "repo-epoch".into(),
                        host: "example.com".into(),
                        identity: "sha256:0123456789abcdef".into(),
                        transport: "https".into(),
                    },
                },
            ],
        };
        assert!(
            !Executor::execution_bound_request(&raw_read),
            "credential.raw.read is never a reusable Git grant"
        );
    }
}
