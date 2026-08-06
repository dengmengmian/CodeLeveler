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
    ApprovalDecision, ApprovalRequest, CommandView, Requirement, ReviewVerdict, RiskLevel,
    command_is_destructive,
};
use leveler_lifecycle::PlanStep;
use leveler_model::{ContentPart, ToolCall};
use leveler_tools::{ToolContext, ToolError};

use super::dispatch::{collect_modified, extract_image, extract_plan};
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
    /// The effective execution context, including any post-approval elevation
    /// (host-escape openers). Private: only the host dereferences it.
    ctx: ToolContext,
}

impl AdmittedCall {
    /// Hand the call back for the loop's post-execution bookkeeping.
    pub(crate) fn into_call(self) -> ToolCall {
        self.call
    }
}

/// An `ApproveAlways` decision waiting to become a durable permission rule.
/// Held until the barrier confirms the approval resolution is on disk, so a
/// crash can never leave a permanent grant that the event log does not explain.
pub(crate) struct PendingStandingGrant {
    tool: String,
    command_line: Option<String>,
    paths: Vec<String>,
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
        ctx: ToolContext,
        parallel: bool,
        session_approved: &mut HashSet<String>,
        cancellation: &CancellationToken,
    ) -> Result<AdmittedCall, AdmitError> {
        // Side-effect barrier, first wait: the announcing `ToolCallStarted`
        // must be durable before ANYTHING can act on this call — the pre-tool
        // hooks below are external side effects themselves. A flush failure
        // aborts the run before the tool executes (never a fake success).
        if let Some(barrier) = &self.event_barrier
            && let Err(error) = barrier.flush().await
        {
            return Err(AdmitError::Fatal(error));
        }
        let mut pending_always: Option<PendingStandingGrant> = None;
        if let Err(reason) = self
            .authorize_with_cancellation(&call, session_approved, &mut pending_always, cancellation)
            .await
        {
            return Err(AdmitError::Refused { call, reason });
        }
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
            return Err(AdmitError::Fatal(error));
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
                return Err(AdmitError::Fatal(error));
            }
            self.remember_always(&grant.tool, grant.command_line.as_deref(), &grant.paths);
        }
        // Host openers (`open`/`xdg-open`) only work outside seatbelt;
        // elevate after approval (the user already OK'd this call).
        let mut ctx = ctx;
        if call_needs_host_escape(&call) {
            ctx.turn_unrestricted_fs = true;
        }
        Ok(AdmittedCall { call, ctx })
    }

    /// Decide whether a tool call may proceed. Returns `Ok(())` to allow, or
    /// `Err(reason)` to reject (fed back to the model as a tool error).
    ///
    /// Order: Pre hooks → permission rules → profile policy → grants/approver.
    pub(crate) async fn authorize_with_cancellation(
        &self,
        call: &ToolCall,
        session_approved: &mut HashSet<String>,
        pending_always: &mut Option<PendingStandingGrant>,
        cancellation: &CancellationToken,
    ) -> Result<(), String> {
        let args_json = serde_json::to_string(&call.arguments).unwrap_or_else(|_| "{}".into());
        match self
            .hook_runner
            .run_pre(&call.name, &args_json, cancellation)
            .await
        {
            leveler_execution::PreHookResult::Allow => {}
            leveler_execution::PreHookResult::Deny(reason) => return Err(reason),
        }

        let risk = self
            .registry
            .get(&call.name)
            .map(|t| t.risk())
            .unwrap_or(RiskLevel::Safe);

        // Extract command for run_command / shell_command so the policy can
        // classify it. shell_command uses a platform wrapper for classification
        // but permission rules match the raw `cmd` string.
        let (program, args) = extract_command(call);
        let command_view = program.as_ref().map(|p| CommandView {
            program: p,
            args: &args,
        });
        let command_line = command_line_for_match(call, program.as_deref(), &args);

        // A tool's declared risk is static: `run_command` carries the same level
        // whether it runs `ls` or `rm -rf`. Name the deletion in the prompt, so
        // it does not read as harmless as a listing.
        let risk = match command_view.as_ref().map(command_is_destructive) {
            Some(true) if risk < RiskLevel::Destructive => RiskLevel::Destructive,
            _ => risk,
        };

        // Paths the call touches, for `path_glob` rules, the approval prompt,
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
                return Err("forbidden by permission rule".to_string());
            }
            leveler_execution::RuleDecision::Allow => return Ok(()),
            leveler_execution::RuleDecision::Ask | leveler_execution::RuleDecision::NoMatch => {}
        }

        let requirement =
            self.approval_policy
                .evaluate(self.tool_context.mode, &call.name, risk, command_view);

        match requirement {
            Requirement::Auto => Ok(()),
            Requirement::Forbidden => Err("forbidden by policy".to_string()),
            Requirement::NeedApproval => {
                let signature = approval_signature(&call.name, program.as_deref(), &args);
                if session_approved.contains(&signature) {
                    return Ok(());
                }
                // Only say something the tool name and command do not already
                // say. "<tool> requested by the model" is filler, and filler in
                // a decision prompt trains people to stop reading it.
                let description = if call_needs_host_escape(call) {
                    format!("{} 会打开工作区之外的应用或文件", call.name)
                } else {
                    String::new()
                };
                let request = ApprovalRequest {
                    id: ApprovalId::generate(),
                    turn_id: None,
                    call_id: call.id.to_string(),
                    action_fingerprint: action_fingerprint(call),
                    tool: call.name.clone(),
                    risk,
                    description,
                    command: command_line.clone(),
                    paths: rule_paths,
                };
                let review = tokio::select! {
                    biased;
                    _ = cancellation.cancelled() => return Err("cancelled".to_string()),
                    verdict = self.auto_reviewer.review(&request) => verdict,
                };
                match review {
                    ReviewVerdict::Allow => return Ok(()),
                    ReviewVerdict::Deny(reason) => return Err(reason),
                    ReviewVerdict::NeedUser => {}
                }
                let decision = tokio::select! {
                    biased;
                    _ = cancellation.cancelled() => return Err("cancelled".to_string()),
                    decision = self.approver.decide(&request) => decision,
                };
                match decision {
                    ApprovalDecision::ApproveOnce => Ok(()),
                    ApprovalDecision::ApproveSession => {
                        // Strictly session-scoped: durable standing permission
                        // is ApproveAlways writing a permission rule.
                        session_approved.insert(signature);
                        Ok(())
                    }
                    ApprovalDecision::ApproveAlways => {
                        // Do NOT write the standing permission here. The
                        // decision is only queued for persistence at this
                        // point; writing a durable rule now means a crash can
                        // leave a permanent grant in place while the event log
                        // still shows the approval unresolved. `admit` writes
                        // it after the barrier confirms the resolution landed.
                        *pending_always = Some(PendingStandingGrant {
                            tool: call.name.clone(),
                            command_line: command_line.clone(),
                            paths: scoped_paths.clone(),
                        });
                        // Session grant too: the current action proceeds even
                        // when no durable rule could be persisted.
                        session_approved.insert(signature);
                        Ok(())
                    }
                    ApprovalDecision::Deny if !self.approver.has_human() => {
                        // Nobody was asked, so nobody refused. Reporting this as
                        // a user decision teaches the model that this user
                        // rejects memories they never saw.
                        Err(self.park_unattended_denial(call))
                    }
                    ApprovalDecision::Deny => Err("denied by user".to_string()),
                }
            }
        }
    }

    #[cfg(test)]
    pub(crate) async fn authorize(
        &self,
        call: &ToolCall,
        session_approved: &mut HashSet<String>,
    ) -> Result<(), String> {
        let mut pending = None;
        let result = self
            .authorize_with_cancellation(
                call,
                session_approved,
                &mut pending,
                &CancellationToken::new(),
            )
            .await;
        // The inline tests assert on the durable rule file, so apply what a
        // real run would apply after its barrier.
        if let Some(grant) = pending {
            self.remember_always(&grant.tool, grant.command_line.as_deref(), &grant.paths);
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
        let Some(root) = self.tool_context.memory_root.as_ref() else {
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
            Ok(leveler_memory::ProposeOutcome::Pending(candidate)) => format!(
                "{UNATTENDED}; kept as a pending candidate [{}] — run `leveler memory accept {}` \
                 to make it durable",
                candidate.id, candidate.id
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
    fn remember_always(&self, tool: &str, command_line: Option<&str>, paths: &[String]) {
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
    /// workspace_snapshot, plan)` to feed back to the model. Infrastructure
    /// errors are converted to model-visible text so the model can react
    /// rather than the loop aborting. Also records any files the tool
    /// modified.
    pub(crate) async fn dispatch(
        &self,
        admitted: &AdmittedCall,
        modified_files: &mut Vec<String>,
        cancellation: &CancellationToken,
    ) -> (
        String,
        bool,
        Option<ContentPart>,
        Option<String>,
        Option<Vec<PlanStep>>,
    ) {
        let (content, is_error, metadata) = self.dispatch_raw(admitted, cancellation).await;
        collect_modified(&metadata, modified_files);
        let image = extract_image(&metadata);
        let snapshot = metadata
            .get("workspace_snapshot")
            .and_then(serde_json::Value::as_str)
            .map(ToOwned::to_owned);
        let plan = extract_plan(&metadata);
        (content, is_error, image, snapshot, plan)
    }

    /// Execute one admitted call, returning `(content, is_error, metadata)`
    /// without touching shared state — safe to run concurrently for
    /// parallel-safe tools. The caller folds `metadata` (modified files,
    /// images) back in call order.
    pub(crate) async fn dispatch_raw(
        &self,
        admitted: &AdmittedCall,
        cancellation: &CancellationToken,
    ) -> (String, bool, serde_json::Value) {
        let call = &admitted.call;
        match self
            .registry
            .execute(
                &call.name,
                call.arguments.clone(),
                admitted.ctx.clone(),
                cancellation.child_token(),
            )
            .await
        {
            Ok(output) => (output.content, output.is_error, output.metadata),
            Err(ToolError::NotFound(name)) if name == "task" => (
                "tool error: unsupported tool `task`; use `spawn_agent` for delegation".to_string(),
                true,
                serde_json::Value::Null,
            ),
            Err(e) => (format!("tool error: {e}"), true, serde_json::Value::Null),
        }
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

    /// `rm -rf …` classifies dangerous (irreversible destruction), so Assisted
    /// always asks for it. (`git push` no longer prompts: sandbox-first.)
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
        let tool_context = ToolContext::new(workspace, PermissionProfile::Assisted)
            .with_memory_root(dir.path().join("memory"));
        let executor = Executor::new(
            Arc::new(StubRuntime),
            Arc::new(default_registry()),
            tool_context,
            ModelRef::new("mock", "m"),
            10,
        )
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
        let tool_context = ToolContext::new(workspace, PermissionProfile::Assisted)
            .with_memory_root(memory_root.clone());
        let executor = Executor::new(
            Arc::new(StubRuntime),
            Arc::new(default_registry()),
            tool_context,
            ModelRef::new("mock", "m"),
            10,
        )
        .with_approver(Arc::new(HeadlessDeny));
        let mut session = HashSet::new();

        let err = executor
            .authorize(&remember_call(), &mut session)
            .await
            .expect_err("the call itself still does not store active memory");
        assert!(
            err.contains("accept"),
            "the message must point at how to adopt it later: {err}"
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
            set.rules()[0].match_.command_prefix.as_deref(),
            Some("rm -rf")
        );

        // A fresh session set is auto-allowed by the live rule set — the
        // approver is not asked again.
        let mut fresh_session = HashSet::new();
        executor
            .authorize(&rm_rf_call(), &mut fresh_session)
            .await
            .unwrap();
        assert_eq!(approver.asks(), 1);
    }

    #[tokio::test]
    async fn approve_session_stays_in_session_and_writes_no_grants_file() {
        let dir = tempfile::tempdir().unwrap();
        let approver = Arc::new(FixedApprover::new(ApprovalDecision::ApproveSession));
        let executor = executor_for(dir.path(), approver.clone()).with_grants_state_dir(dir.path());
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
    async fn approve_always_memory_write_stays_session_only() {
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
        assert!(
            !leveler_execution::project_rules_path(dir.path()).exists(),
            "K36: memory writes never get standing permission"
        );
    }

    #[tokio::test]
    async fn path_glob_deny_rule_matches_call_paths() {
        let dir = tempfile::tempdir().unwrap();
        let approver = Arc::new(FixedApprover::new(ApprovalDecision::ApproveOnce));
        let deny_src = leveler_execution::PermissionRule {
            match_: leveler_execution::RuleMatch {
                tool: Some("apply_patch".into()),
                command_prefix: None,
                command_exact: None,
                path_glob: Some("src/**".into()),
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
}
