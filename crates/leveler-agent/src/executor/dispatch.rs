use std::collections::HashSet;

use tokio_util::sync::CancellationToken;

use leveler_core::ApprovalId;
use leveler_execution::{
    ApprovalDecision, ApprovalRequest, CommandView, Requirement, ReviewVerdict, RiskLevel,
};
use leveler_lifecycle::{EvidenceLedger, PlanState, PlanStep};
use leveler_model::{ContentPart, ImageSource, ToolCall, ToolResultContent};
use leveler_tools::{ToolContext, ToolError};

use super::{AgentEvent, Executor};
use crate::authorization::{
    action_fingerprint, approval_signature, call_needs_host_escape, collect_scoped_paths_from_call,
    command_line_for_match, extract_command,
};

impl Executor {
    /// Decide whether a tool call may proceed. Returns `Ok(())` to allow, or
    /// `Err(reason)` to reject (fed back to the model as a tool error).
    ///
    /// Order: Pre hooks → permission rules → profile policy → grants/approver.
    pub(crate) async fn authorize_with_cancellation(
        &self,
        call: &ToolCall,
        session_approved: &mut HashSet<String>,
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

        // Paths the call touches, for `path_glob` rules, the approval prompt,
        // and deriving `ApproveAlways` path rules.
        let mut scoped_paths: Vec<String> = Vec::new();
        collect_scoped_paths_from_call(call, &mut scoped_paths);
        let rule_paths: Vec<std::path::PathBuf> =
            scoped_paths.iter().map(std::path::PathBuf::from).collect();

        let rule_decision = self.permission_rules.read().unwrap().evaluate(
            &call.name,
            command_line.as_deref(),
            &rule_paths,
        );
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
                let description = if call_needs_host_escape(call) {
                    format!(
                        "{} opens a host app/file outside the workspace sandbox — needs your OK",
                        call.name
                    )
                } else {
                    format!("{} requested by the model", call.name)
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
                        self.remember_always(&call.name, command_line.as_deref(), &scoped_paths);
                        // Session grant too: the current action proceeds even
                        // when no durable rule could be persisted.
                        session_approved.insert(signature);
                        Ok(())
                    }
                    ApprovalDecision::Deny => Err("denied by user".to_string()),
                }
            }
        }
    }

    #[cfg(test)]
    async fn authorize(
        &self,
        call: &ToolCall,
        session_approved: &mut HashSet<String>,
    ) -> Result<(), String> {
        self.authorize_with_cancellation(call, session_approved, &CancellationToken::new())
            .await
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
            .unwrap()
            .extend(leveler_execution::PermissionRuleSet::from_rules(rules));
    }

    /// Execute one tool call, returning `(content, is_error, image)` to feed
    /// back to the model. Infrastructure errors are converted to model-visible
    /// text so the model can react rather than the loop aborting. Also records
    /// any files the tool modified.
    pub(crate) async fn dispatch(
        &self,
        call: &ToolCall,
        ctx: ToolContext,
        modified_files: &mut Vec<String>,
        cancellation: &CancellationToken,
    ) -> (
        String,
        bool,
        Option<ContentPart>,
        Option<String>,
        Option<Vec<PlanStep>>,
    ) {
        let (content, is_error, metadata) = self.dispatch_raw(call, ctx, cancellation).await;
        collect_modified(&metadata, modified_files);
        let image = extract_image(&metadata);
        let snapshot = metadata
            .get("workspace_snapshot")
            .and_then(serde_json::Value::as_str)
            .map(ToOwned::to_owned);
        let plan = extract_plan(&metadata);
        (content, is_error, image, snapshot, plan)
    }

    /// Execute one tool call, returning `(content, is_error, metadata)` without
    /// touching shared state — safe to run concurrently for parallel-safe tools.
    /// The caller folds `metadata` (modified files, images) back in call order.
    pub(crate) async fn dispatch_raw(
        &self,
        call: &ToolCall,
        ctx: ToolContext,
        cancellation: &CancellationToken,
    ) -> (String, bool, serde_json::Value) {
        match self
            .registry
            .execute(
                &call.name,
                call.arguments.clone(),
                ctx,
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

/// Pull the structured plan an update_plan call exposed via `metadata.plan`,
/// so the executor can surface it as [`AgentEvent::PlanUpdated`].
pub(crate) fn extract_plan(metadata: &serde_json::Value) -> Option<Vec<PlanStep>> {
    let steps: Vec<PlanStep> = serde_json::from_value(metadata.get("plan")?.clone()).ok()?;
    (!steps.is_empty()).then_some(steps)
}

/// Pull a base64 image a tool exposed via `metadata.image` into an image content
/// part, so the executor can show it to a vision model on the next request.
pub(crate) fn extract_image(metadata: &serde_json::Value) -> Option<ContentPart> {
    let img = metadata.get("image")?;
    let media_type = img.get("media_type")?.as_str()?.to_string();
    let data = img.get("data")?.as_str()?.to_string();
    Some(ContentPart::Image {
        source: ImageSource::Base64 { media_type, data },
    })
}

pub(crate) fn collect_modified(metadata: &serde_json::Value, out: &mut Vec<String>) {
    if let Some(files) = metadata.get("modified_files").and_then(|v| v.as_array()) {
        for f in files {
            if let Some(s) = f.as_str()
                && !out.iter().any(|e| e == s)
            {
                out.push(s.to_string());
            }
        }
    }
}

/// Paths present in `after` but not in `before` (this tool call's net writes).
pub(crate) fn newly_modified_paths(before: &[String], after: &[String]) -> Vec<String> {
    after
        .iter()
        .filter(|p| !before.iter().any(|e| e == *p))
        .cloned()
        .collect()
}

/// Record mutation evidence for any tool that produced new modified_files.
pub(crate) fn note_tool_side_effects(
    ledger: &mut EvidenceLedger,
    tool_call_id: &str,
    tool: &str,
    newly: Vec<String>,
    plan_state: &PlanState,
    observer: &mut dyn FnMut(AgentEvent),
) {
    if newly.is_empty() {
        return;
    }
    ledger.record_mutation(tool_call_id, tool, newly);
    ledger.plan = plan_state.clone();
    observer(AgentEvent::EvidenceLedgerUpdated {
        ledger: ledger.clone(),
    });
}

/// The text a user's request carries. Images and other parts hold no language.
pub(crate) fn text_of(content: &[ContentPart]) -> String {
    content
        .iter()
        .filter_map(|part| match part {
            ContentPart::Text { text } => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

pub(crate) fn compact_json(value: &serde_json::Value) -> String {
    preview(&value.to_string())
}

/// Read-only tools permitted during plan explore rounds (before first plan).
pub(crate) fn is_plan_explore_tool(name: &str) -> bool {
    matches!(
        name,
        "read_file"
            | "list_files"
            | "grep"
            | "repository_search"
            | "find_symbol"
            | "read_symbol"
            | "find_references"
            | "web_search"
            | "web_fetch"
            | "load_skill"
    )
}

/// Keep one-step requests lightweight, but require a machine-readable plan for
/// requests that already spell out several independently checkable pieces of
/// work. The gate intentionally uses only obvious structure; uncertain tasks
/// may still create a plan voluntarily without blocking simple edits.
pub(crate) fn task_needs_structured_plan(task: &str) -> bool {
    fn is_list_item(line: &str) -> bool {
        let line = line.trim_start();
        if line.starts_with("- ") || line.starts_with("* ") || line.starts_with("• ") {
            return true;
        }
        let digits = line.chars().take_while(char::is_ascii_digit).count();
        digits > 0
            && matches!(
                line.as_bytes().get(digits).copied(),
                Some(b'.' | b')' | b':')
            )
    }

    if task
        .lines()
        .filter(|line| is_list_item(line))
        .take(2)
        .count()
        >= 2
    {
        return true;
    }

    let sentence_count = task
        .split(['。', '！', '？', '.', '!', '?', '\n'])
        .filter(|part| !part.trim().is_empty())
        .take(4)
        .count();
    let normalized = task.to_lowercase();
    let concern_markers = [
        "而且",
        "还要",
        "然后",
        "最后",
        "并且",
        "同时",
        "also",
        "and also",
        "finally",
        "additionally",
    ]
    .into_iter()
    .filter(|marker| normalized.contains(marker))
    .take(2)
    .count();
    task.chars().count() >= 60 && sentence_count >= 3 && concern_markers >= 2
}

/// Refuse a call a guard stopped before it ran, and feed the reason back to the
/// model as the call's result.
///
/// The call is announced first even though it never executes: a `ToolResult`
/// whose id no `ToolCall` ever introduced reaches the UI as an id it has never
/// seen, leaving it with no name or arguments to render — the row comes out
/// blank. A refusal has to say what was refused.
pub(crate) fn deny_call(
    observer: &mut dyn FnMut(AgentEvent),
    call: ToolCall,
    message: String,
) -> ContentPart {
    observer(AgentEvent::ToolCall {
        id: call.id.as_str().to_string(),
        name: call.name.clone(),
        arguments: compact_json(&call.arguments),
    });
    observer(AgentEvent::ToolResult {
        id: call.id.as_str().to_string(),
        name: call.name.clone(),
        is_error: true,
        preview: message.clone(),
    });
    ContentPart::ToolResult {
        result: ToolResultContent {
            call_id: call.id,
            content: message,
            is_error: true,
        },
    }
}

pub(crate) fn preview(s: &str) -> String {
    const MAX: usize = 1200;
    // Drop ANSI before truncating so color codes neither pollute the TUI nor
    // burn the preview budget.
    let clean = leveler_core::sanitize_terminal_output(s);
    if clean.chars().count() <= MAX {
        clean
    } else {
        let truncated: String = clean.chars().take(MAX).collect();
        format!("{truncated}…")
    }
}

#[cfg(test)]
mod mutation_ledger_tests {
    use super::*;

    #[test]
    fn newly_modified_paths_only_returns_delta() {
        let before = vec!["a.rs".into(), "b.rs".into()];
        let after = vec!["a.rs".into(), "b.rs".into(), "c.rs".into()];
        assert_eq!(
            newly_modified_paths(&before, &after),
            vec!["c.rs".to_string()]
        );
        assert!(newly_modified_paths(&after, &after).is_empty());
    }

    #[test]
    fn note_tool_side_effects_records_run_command_mutations() {
        let mut ledger = EvidenceLedger::default();
        let plan = PlanState::default();
        let mut events = 0u32;
        note_tool_side_effects(
            &mut ledger,
            "c1",
            "run_command",
            vec!["generated.rs".into()],
            &plan,
            &mut |_| {
                events += 1;
            },
        );
        assert_eq!(ledger.mutations.len(), 1);
        assert_eq!(ledger.mutations[0].tool, "run_command");
        assert_eq!(ledger.mutations[0].paths, vec!["generated.rs".to_string()]);
        assert_eq!(events, 1);
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
}
