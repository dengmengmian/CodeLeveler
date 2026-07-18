use std::collections::HashSet;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use tokio_util::sync::CancellationToken;

use leveler_context::{load_scoped_rules, render_instructions};
use leveler_lifecycle::{
    CompleteStepReceipt, DepthUseMetrics, EvidenceLedger, GateConfig, ObjectiveAnchor, PlanState,
    ProgressCaps, TaskContract, TurnPhase, check, task_looks_like_implementation,
};
use leveler_model::{
    ContentPart, FinishReason, Message, ModelError, ModelRequest, Role, ToolCall, ToolChoice,
    ToolResultContent,
};
use leveler_tools::ToolContext;

use super::dispatch::{
    collect_modified, compact_json, deny_call, extract_image, extract_plan, is_plan_explore_tool,
    newly_modified_paths, note_tool_side_effects, preview, task_needs_structured_plan,
};
use super::{
    AgentError, AgentEvent, AgentOutcome, AnswerAudit, Executor, LOOP_GUARD_THRESHOLD,
    ModelRequestRecord, StopReason, TranscriptSink,
};
use crate::authorization::{
    call_needs_host_escape, collect_scoped_paths_from_call, counts_as_verification_evidence,
    extract_command, is_pure_observe_call, is_search_tool, observe_class, push_unique_path,
    write_targets_outside_allowlist,
};
use crate::compaction::{COMPACT_KEEP_RECENT, compact_messages, estimate_tokens};
use crate::injected_tools::{
    COMPLETE_STEP_TOOL, REQUEST_PERMISSIONS_TOOL, SPAWN_AGENT_TOOL, UPDATE_GOAL_TOOL,
    apply_turn_grants, ask_user_tool_definition, complete_step_tool_definition, is_user_input_tool,
    request_permissions_tool_definition, request_user_input_tool_definition,
    spawn_agent_tool_definition, update_goal_tool_definition,
};
use crate::nudges::{
    STEP_SUMMARY_NUDGE, completion_audit_nudge, first_user_text, goal_resolve_nudge,
};
use crate::sub_agent::{AgentRole, MAX_SUB_AGENT_DEPTH, agent_nickname};

struct CancelOnDrop(CancellationToken);

impl Drop for CancelOnDrop {
    fn drop(&mut self) {
        self.0.cancel();
    }
}

impl Executor {
    /// The core loop over a growing message transcript.
    pub(crate) async fn drive(
        &self,
        mut messages: Vec<Message>,
        objective: ObjectiveAnchor,
        observer: &mut dyn FnMut(AgentEvent),
        sink: &mut dyn TranscriptSink,
        cancellation: CancellationToken,
    ) -> Result<AgentOutcome, AgentError> {
        let mut tools = self.registry.definitions();
        // Primary name, plus legacy ask_user for older models and prompts.
        tools.push(request_user_input_tool_definition());
        tools.push(ask_user_tool_definition());
        tools.push(request_permissions_tool_definition());
        // A sub-agent shouldn't spawn its own sub-agents.
        if self.depth < MAX_SUB_AGENT_DEPTH {
            tools.push(spawn_agent_tool_definition());
        }
        // Goal mode: the model resolves the objective explicitly.
        if self.goal_mode {
            tools.push(update_goal_tool_definition());
            if self.delivery_gate {
                tools.push(complete_step_tool_definition());
            }
        }
        let mut modified_files: Vec<String> = Vec::new();
        let mut scoped_paths: Vec<String> = Vec::new();
        let mut metrics = DepthUseMetrics::default();
        // Active objective is host-pinned (this turn / session goal).
        let original_task = if objective.is_empty() {
            first_user_text(&messages)
        } else {
            objective.text().to_string()
        };
        let progress_caps = ProgressCaps::default();
        let mut progress = self
            .seeded_progress
            .clone()
            .with_objective_version(objective.version);
        progress.phase = TurnPhase::Active;
        let structured_plan_required =
            self.require_explicit_plan && task_needs_structured_plan(&original_task);
        // Complex tasks may use a few read-only explore rounds before plan.
        const PLAN_EXPLORE_ROUNDS: u32 = 2;
        let mut plan_explore_rounds_used = 0u32;
        let mut plan_state = self.seeded_plan.clone();
        let mut structured_plan_started = !plan_state.is_empty();
        // Short tasks: no host-seeded one-step plan shell. Plan UI appears only when
        // the model calls update_plan, or resume rehydrates a prior PlanUpdated.
        // Complex tasks still require ModelExplicit update_plan before mutation
        // (structured_plan_required + explore budget). HostImplicit remains for
        // resume of older sessions that already persisted that origin.
        // Task contract → user turn (never system prefix) so prefix cache stays stable.
        let task_contract = TaskContract::parse(&original_task);
        let contract_injection = task_contract.user_injection();
        if !contract_injection.trim().is_empty()
            && !messages
                .iter()
                .any(|m| m.role == Role::User && m.text_content().contains("## Task contract"))
        {
            messages.push(Message::text(Role::User, contract_injection));
        }
        let mut session_approved: HashSet<String> = HashSet::new();
        if let Some(dir) = &self.grants_state_dir {
            let file = leveler_execution::load_grants(dir);
            for sig in leveler_execution::signatures_from_file(&file) {
                session_approved.insert(sig);
            }
        }
        // Sub-agents spawned so far this run (bounds total delegation).
        let mut agents_spawned = 0usize;
        // Accumulated elevations from approved request_permissions this turn.
        let mut turn_grants = crate::injected_tools::TurnPermissionGrants::default();
        // Leveling: consecutive search calls with no intervening action. Reset
        // whenever the model does something concrete (read/edit/run).
        let mut consecutive_searches = 0usize;
        // Sources of scoped AGENTS.md rules already appended to the transcript,
        // so a second file in the same directory does not re-inject them.
        let mut injected_rule_sources: Vec<String> = Vec::new();
        // The most recent non-empty assistant text, surfaced if the round budget
        // runs out so a truncated run still reports where it got to.
        let mut last_text = String::new();
        // Completion-evidence gate: whether a verification command has passed,
        // and how many times we have refused an unverified completion.
        let mut verification_ran = false;
        // Delivery EvidenceLedger (mutation/verify/complete_step receipts).
        // Resume may seed prior mutations/verifies from last EvidenceLedgerUpdated.
        let mut ledger = {
            let mut led = self.seeded_ledger.clone();
            if !plan_state.is_empty() {
                led.plan = plan_state.clone();
            }
            led
        };
        let mut evidence_nudges = 0u32;
        let mut progress_since_evidence_nudge = true;
        // No-progress loop guard: "name\0args" -> (last result content, identical
        // repeat count). Blocks a call that keeps producing the same output.
        let mut call_history: std::collections::HashMap<String, (String, u32)> =
            std::collections::HashMap::new();
        // Bounded recovery from a malformed tool call: a weak model sometimes
        // emits `run_command`/tool arguments that aren't valid JSON (an
        // unescaped backslash from a regex, or a raw newline from a multi-line
        // script). Rather than failing the whole turn on that Decode error,
        // feed the parse error back and let the model resend. Reset on any
        // clean round so the cap is on *consecutive* failures.
        let mut decode_retries = 0u32;
        const MAX_DECODE_RETRIES: u32 = 2;
        // Plain-text output may hit the provider's per-response limit. Continue
        // the same answer a bounded number of times; truncated tool calls are
        // never safe to execute and fail immediately below.
        let mut length_continuations = 0u32;
        let mut continued_text = String::new();
        const MAX_LENGTH_CONTINUATIONS: u32 = 2;
        let mut had_tool_calls = false;
        let mut answer_audit_repairs = 0u32;
        const MAX_ANSWER_AUDIT_REPAIRS: u32 = 2;
        // Goal mode: how many times the model went quiet without calling
        // update_goal. Each time we re-prompt it to audit and resolve explicitly;
        // after a small cap we accept completion, so a model that never learns to
        // call update_goal still terminates as stalled instead of running forever.
        let mut goal_quiet_nudges = 0u32;
        const MAX_GOAL_QUIET_NUDGES: u32 = 3;
        // Closeout thrash nudge (once): plan complete / delivery closeout.
        let mut post_plan_closeout_nudged = false;
        let mut no_progress_nudge_sent = false;
        // Tools seen this round for pure-observe streak detection.
        #[allow(unused_assignments)]
        let mut observe_only_tools_this_round = 0u32;
        #[allow(unused_assignments)]
        let mut non_observe_success_this_round = 0u32;
        // Hard step limits (spec §27): wall clock from run start, commands
        // executed so far, and the reason once a limit trips. The round that
        // trips a limit still commits its tool results (well-formed transcript)
        // and then the run returns BudgetExhausted.
        // Epoch spend: continue/resume seeds prior cumulative totals so limits
        // are task-level, not per-drive zeros.
        let run_started = std::time::Instant::now();
        let epoch_duration_at_start =
            std::time::Duration::from_millis(progress.cumulative_duration_ms);
        // Turn-wide deadline: cancel the same token passed to model streams,
        // hooks, reviewers, approvers and tools. The existing round-boundary
        // budget check then produces the normal BudgetExhausted outcome.
        let deadline_expired = Arc::new(AtomicBool::new(false));
        let turn_cancellation = cancellation.child_token();
        let deadline_done = CancellationToken::new();
        let _deadline_guard = CancelOnDrop(deadline_done.clone());
        if let Some(max) = self.step_limits.max_duration {
            let remaining = max.saturating_sub(epoch_duration_at_start);
            let expired = Arc::clone(&deadline_expired);
            let deadline_token = turn_cancellation.clone();
            let external = cancellation.clone();
            tokio::spawn(async move {
                tokio::select! {
                    _ = external.cancelled() => {}
                    _ = deadline_done.cancelled() => {}
                    _ = tokio::time::sleep(remaining) => {
                        expired.store(true, Ordering::Release);
                        deadline_token.cancel();
                    }
                }
            });
        }
        let cancellation = turn_cancellation;
        let mut commands_run = progress.cumulative_commands;
        let mut model_tokens_spent = progress.cumulative_model_tokens;
        let mut cost_spent_micros = progress.cumulative_cost_usd_micros;
        let mut budget_exceeded: Option<String> = None;
        let epoch_tokens_at_start = progress.cumulative_model_tokens;
        let epoch_rounds_at_start = progress.cumulative_rounds;

        if self.step_limits.max_cost_usd_micros.is_some() && self.pricing.is_none() {
            return Err(AgentError::InvalidBudget(
                "a cost limit requires pricing in the selected model profile".to_string(),
            ));
        }

        let mut round = 0u32;
        loop {
            if let Some(max) = self.step_limits.max_model_tokens
                && model_tokens_spent >= max
            {
                let reason = format!(
                    "Stopped: the {max}-token model budget was exhausted after {round} round(s)."
                );
                observer(AgentEvent::Finished(reason.clone()));
                sync_epoch_progress(
                    &mut progress,
                    &mut metrics,
                    epoch_rounds_at_start,
                    epoch_tokens_at_start,
                    epoch_duration_at_start,
                    run_started,
                    round,
                    model_tokens_spent,
                    commands_run,
                    cost_spent_micros,
                    &modified_files,
                );
                observer(AgentEvent::ProgressUpdated {
                    ledger: progress.clone(),
                });

                return Ok(AgentOutcome {
                    final_text: reason,
                    rounds: round,
                    modified_files,
                    stop_reason: StopReason::BudgetExhausted,
                    stop_detail: Some("model token budget exhausted".to_string()),
                    metrics: metrics.clone(),
                    progress: progress.clone(),
                    objective: objective.clone(),
                });
            }
            if let Some(max) = self.step_limits.max_cost_usd_micros
                && cost_spent_micros >= max
            {
                let reason = format!(
                    "Stopped: the {max}-micro-USD model cost budget was exhausted after {round} round(s)."
                );
                observer(AgentEvent::Finished(reason.clone()));
                sync_epoch_progress(
                    &mut progress,
                    &mut metrics,
                    epoch_rounds_at_start,
                    epoch_tokens_at_start,
                    epoch_duration_at_start,
                    run_started,
                    round,
                    model_tokens_spent,
                    commands_run,
                    cost_spent_micros,
                    &modified_files,
                );
                observer(AgentEvent::ProgressUpdated {
                    ledger: progress.clone(),
                });

                return Ok(AgentOutcome {
                    final_text: reason,
                    rounds: round,
                    modified_files,
                    stop_reason: StopReason::BudgetExhausted,
                    stop_detail: Some("model cost budget exhausted".to_string()),
                    metrics: metrics.clone(),
                    progress: progress.clone(),
                    objective: objective.clone(),
                });
            }
            if self
                .continuation
                .round_limit()
                .is_some_and(|max| round >= max)
            {
                break;
            }
            round = round.saturating_add(1);
            let has_next_round = self.continuation.allows_round_after(round);
            if cancellation.is_cancelled() && !deadline_expired.load(Ordering::Acquire) {
                // Flush epoch spend before Cancelled so resume/event-log keep
                // command/file/token totals (including any absorbed children).
                sync_epoch_progress(
                    &mut progress,
                    &mut metrics,
                    epoch_rounds_at_start,
                    epoch_tokens_at_start,
                    epoch_duration_at_start,
                    run_started,
                    round,
                    model_tokens_spent,
                    commands_run,
                    cost_spent_micros,
                    &modified_files,
                );
                observer(AgentEvent::ProgressUpdated {
                    ledger: progress.clone(),
                });
                return Err(AgentError::Cancelled);
            }
            if let Some(max) = self.step_limits.max_duration {
                let elapsed = epoch_duration_at_start.saturating_add(run_started.elapsed());
                // Some(0) = hard exhausted residual; elapsed > max for positive caps.
                if max.is_zero() || elapsed > max {
                    let reason = format!(
                        "Stopped: the {}s duration budget was exhausted after {} round(s).",
                        max.as_secs_f64(),
                        round.saturating_sub(1)
                    );
                    observer(AgentEvent::Finished(reason.clone()));
                    sync_epoch_progress(
                        &mut progress,
                        &mut metrics,
                        epoch_rounds_at_start,
                        epoch_tokens_at_start,
                        epoch_duration_at_start,
                        run_started,
                        round,
                        model_tokens_spent,
                        commands_run,
                        cost_spent_micros,
                        &modified_files,
                    );
                    observer(AgentEvent::ProgressUpdated {
                        ledger: progress.clone(),
                    });

                    return Ok(AgentOutcome {
                        final_text: reason,
                        rounds: round - 1,
                        modified_files,
                        stop_reason: StopReason::BudgetExhausted,
                        stop_detail: None,
                        metrics: metrics.clone(),
                        progress: progress.clone(),
                        objective: objective.clone(),
                    });
                }
            }

            // Nested AGENTS.md rules for directories touched so far. Appended at
            // the tail rather than folded into the system prompt: rewriting the
            // first message would invalidate the provider's prefix cache for the
            // entire transcript on every round.
            let fresh = load_scoped_rules(
                self.tool_context.workspace.root(),
                &scoped_paths,
                &injected_rule_sources,
            );
            if !fresh.is_empty() {
                injected_rule_sources.extend(fresh.iter().map(|r| r.source.clone()));
                messages.push(Message::text(
                    Role::System,
                    format!("Project rules:\n{}", render_instructions(&fresh)),
                ));
            }

            let mut request = ModelRequest::new(self.model.clone(), messages.clone());
            request.tools = tools.clone();
            request.tool_choice = ToolChoice::Auto;
            request.max_output_tokens = Some(self.max_output_tokens);
            request.reasoning_effort = self.reasoning_effort;

            let stream_result = match self
                .stream_round_with_retry(request, observer, &cancellation)
                .await
            {
                Ok(v) => {
                    decode_retries = 0;
                    v
                }
                // Malformed tool-call JSON: feed the error back and retry the
                // round instead of aborting, up to a small cap.
                Err(AgentError::Model(e))
                    if e.kind == leveler_model::ModelErrorKind::Decode
                        && decode_retries < MAX_DECODE_RETRIES
                        && !cancellation.is_cancelled() =>
                {
                    decode_retries += 1;
                    let feedback = Message::text(
                        Role::User,
                        format!(
                            "你上一次的工具调用参数不是合法 JSON:{}。请重新发起同一个工具调用,\
                             确保 arguments 是严格合法的 JSON——字符串里的反斜杠写成 `\\\\`、\
                             换行写成 `\\n`,不要放裸换行或裸反斜杠。多行脚本请拆成单行,\
                             或改用 write_file / apply_patch 之类不必在命令里塞长文本的工具。",
                            e.message
                        ),
                    );
                    sink.append(std::slice::from_ref(&feedback)).await?;
                    messages.push(feedback);
                    continue;
                }
                Err(AgentError::Cancelled) if deadline_expired.load(Ordering::Acquire) => {
                    // Re-enter at the round boundary, which records progress
                    // and returns the standard duration-budget outcome.
                    continue;
                }
                Err(e) => return Err(e),
            };

            sink.record_model_request(&ModelRequestRecord {
                id: stream_result.request_id.clone(),
                provider: self.model.provider.clone(),
                model: self.model.model.clone(),
                usage: stream_result.usage,
                finish_reason: stream_result.finish_reason,
                latency_ms: stream_result.latency_ms,
                retry_count: stream_result.retry_count,
            })
            .await?;
            model_tokens_spent = model_tokens_spent.saturating_add(stream_result.usage.total());
            if let Some(pricing) = self.pricing {
                cost_spent_micros = cost_spent_micros.saturating_add(pricing.cost_usd_micros(
                    stream_result.usage.input_tokens,
                    stream_result.usage.output_tokens,
                ));
            }
            // Cost can cross the limit on the response that tips it; stop after
            // this round's tools (if any) rather than allowing another model call.
            if let Some(max) = self.step_limits.max_cost_usd_micros
                && cost_spent_micros >= max
            {
                budget_exceeded = Some(format!(
                    "Stopped: the {max}-micro-USD model cost budget was exhausted after {round} round(s)."
                ));
            }
            // Epoch totals for continue/resume inheritance (absolute spend).
            // Persist ProgressUpdated so the next turn's seed gate and budget
            // resume see the same ledger (event log is SoT, not in-memory only).
            // Tool-phase increments are re-synced on every exit via
            // `sync_epoch_progress` so resume never under-counts commands/files.
            sync_epoch_progress(
                &mut progress,
                &mut metrics,
                epoch_rounds_at_start,
                epoch_tokens_at_start,
                epoch_duration_at_start,
                run_started,
                round,
                model_tokens_spent,
                commands_run,
                cost_spent_micros,
                &modified_files,
            );
            observer(AgentEvent::ProgressUpdated {
                ledger: progress.clone(),
            });

            let assistant = stream_result.message;
            let used_tokens = stream_result.usage.total();
            let finish_reason = stream_result.finish_reason;

            let text = assistant.text_content();
            let calls: Vec<ToolCall> = assistant
                .content
                .iter()
                .filter_map(|p| match p {
                    ContentPart::ToolCall { call } => Some(call.clone()),
                    _ => None,
                })
                .collect();

            match finish_reason {
                FinishReason::Length => {
                    if !calls.is_empty() {
                        return Err(AgentError::Model(ModelError::new(
                            leveler_model::ModelErrorKind::Truncated,
                            "model output ended at the token limit while producing a tool call; the call was not executed",
                        )));
                    }
                    if text.trim().is_empty()
                        || length_continuations >= MAX_LENGTH_CONTINUATIONS
                        || !has_next_round
                    {
                        return Err(AgentError::Model(ModelError::new(
                            leveler_model::ModelErrorKind::Truncated,
                            "model output remained truncated after bounded continuation attempts",
                        )));
                    }
                    length_continuations += 1;
                    continued_text.push_str(&text);
                    last_text = continued_text.clone();
                    sink.append(std::slice::from_ref(&assistant)).await?;
                    messages.push(assistant);
                    messages.push(Message::text(
                        Role::User,
                        "Continue exactly from the cutoff. Do not repeat prior text. Complete every open list, code block, sentence, and conclusion.",
                    ));
                    continue;
                }
                FinishReason::ContentFilter => {
                    return Err(AgentError::Model(ModelError::new(
                        leveler_model::ModelErrorKind::ContentFiltered,
                        "provider content filtering stopped the response before a complete answer",
                    )));
                }
                FinishReason::Other => {
                    return Err(AgentError::Model(ModelError::new(
                        leveler_model::ModelErrorKind::Other,
                        "provider returned an unknown terminal finish reason",
                    )));
                }
                FinishReason::ToolCalls if calls.is_empty() => {
                    return Err(AgentError::Model(ModelError::new(
                        leveler_model::ModelErrorKind::Decode,
                        "provider reported tool_calls but supplied no complete tool call",
                    )));
                }
                FinishReason::Stop if !calls.is_empty() => {
                    return Err(AgentError::Model(ModelError::new(
                        leveler_model::ModelErrorKind::Decode,
                        "provider supplied tool calls with an incompatible stop finish reason",
                    )));
                }
                FinishReason::Stop | FinishReason::ToolCalls => {}
            }
            had_tool_calls |= !calls.is_empty();

            if !text.trim().is_empty() {
                if continued_text.is_empty() {
                    last_text = text.clone();
                } else {
                    continued_text.push_str(&text);
                    last_text = continued_text.clone();
                }
                observer(AgentEvent::AssistantText(last_text.clone()));
            }

            messages.push(assistant.clone());

            // Cost tip-over after this response with no tools: end now (no more rounds).
            if calls.is_empty()
                && let Some(reason) = budget_exceeded.take()
            {
                sink.append(&[assistant]).await?;
                observer(AgentEvent::Finished(reason.clone()));
                sync_epoch_progress(
                    &mut progress,
                    &mut metrics,
                    epoch_rounds_at_start,
                    epoch_tokens_at_start,
                    epoch_duration_at_start,
                    run_started,
                    round,
                    model_tokens_spent,
                    commands_run,
                    cost_spent_micros,
                    &modified_files,
                );
                observer(AgentEvent::ProgressUpdated {
                    ledger: progress.clone(),
                });
                return Ok(AgentOutcome {
                    final_text: reason,
                    rounds: round,
                    modified_files,
                    stop_reason: StopReason::BudgetExhausted,
                    stop_detail: Some("model cost budget exhausted".to_string()),
                    metrics: metrics.clone(),
                    progress: progress.clone(),
                    objective: objective.clone(),
                });
            }

            if calls.is_empty() {
                // Goal mode: going quiet does NOT end the run.
                // Re-prompt the model to audit against the current workspace and
                // resolve explicitly via update_goal. Past the nudge cap the run
                // stops as `Stalled` — never as a success — so a model that never
                // learns to call update_goal terminates without the harness
                // declaring completion on its behalf.
                //
                // No-progress is counted once when this drive ends as Stalled
                // (below), not on every quiet nudge — so one drive can still use
                // its nudge budget, while Engine continue is capped across turns.
                if self.goal_mode && goal_quiet_nudges < MAX_GOAL_QUIET_NUDGES && has_next_round {
                    goal_quiet_nudges += 1;
                    metrics.extra_model_calls += 1;
                    sink.append(&[assistant]).await?;
                    messages.push(Message {
                        role: Role::User,
                        content: vec![ContentPart::Text {
                            text: goal_resolve_nudge(&original_task),
                        }],
                    });
                    continue;
                }
                // Completion-evidence gate (spec §17): refuse the first
                // completion that has no verification behind it, once, so the
                // model runs the build/tests instead of declaring success blind.
                if self.require_completion_evidence
                    && !modified_files.is_empty()
                    && !verification_ran
                    && (evidence_nudges == 0 || progress_since_evidence_nudge)
                    && has_next_round
                {
                    evidence_nudges += 1;
                    progress_since_evidence_nudge = false;
                    messages.push(Message {
                        role: Role::User,
                        content: vec![ContentPart::Text {
                            text: completion_audit_nudge(&original_task),
                        }],
                    });
                    continue;
                }
                // Every tool-backed turn is audited against what was actually
                // asked, edits or not. The verification gates answer a different
                // question — "does the code still build and pass?" — and a turn
                // can be green on both while quietly skipping a deliverable it
                // was handed (asked for five things, did four, reported done).
                // Whether a file changed says nothing about whether the request
                // was finished, so it must not decide who gets checked.
                //
                // The audit stays advisory: it may request bounded repairs, but
                // model self-review is not objective evidence that a finished
                // answer failed.
                if self.answer_audit && had_tool_calls && !self.goal_mode {
                    metrics.answer_audit_invocations += 1;
                    metrics.extra_model_calls += 1;
                    match self.audit_answer(&messages, &cancellation).await? {
                        AnswerAudit::Complete => {}
                        AnswerAudit::Missing(missing)
                            if answer_audit_repairs < MAX_ANSWER_AUDIT_REPAIRS
                                && has_next_round =>
                        {
                            answer_audit_repairs += 1;
                            sink.append(&[assistant]).await?;
                            continued_text = last_text.clone();
                            let missing = if missing.is_empty() {
                                "- The audit found the answer incomplete but did not name the missing branch. Re-check the original request and tool evidence.".to_string()
                            } else {
                                missing
                                    .into_iter()
                                    .map(|item| format!("- {item}"))
                                    .collect::<Vec<_>>()
                                    .join("\n")
                            };
                            messages.push(Message::text(
                                Role::User,
                                format!(
                                    "A separate completeness audit found these omissions:\n{missing}\nContinue the answer from where it stopped. Cover those omissions precisely, do not repeat completed sections, and provide a clear closing conclusion."
                                ),
                            ));
                            continue;
                        }
                        AnswerAudit::Missing(missing) => tracing::warn!(
                            missing = ?missing,
                            "answer completeness audit still reports omissions after bounded repairs"
                        ),
                        AnswerAudit::Unavailable(reason) => {
                            tracing::warn!(%reason, "answer completeness audit unavailable");
                        }
                    }
                }
                sink.append(&[assistant]).await?;
                observer(AgentEvent::Finished(last_text.clone()));
                // In goal mode reaching this point means the model went quiet
                // through every nudge without ever calling update_goal — that
                // is a stall, not a proven completion.
                let (stop_reason, stop_detail) = if self.goal_mode {
                    // One no-progress tick per stalled drive so Engine
                    // continue_active_goal cannot open unbounded turns.
                    progress.note_no_progress_round(round);
                    if progress.should_hard_stop_no_progress(progress_caps) {
                        progress.enter_terminal();
                    }
                    observer(AgentEvent::ProgressUpdated {
                        ledger: progress.clone(),
                    });
                    (
                        StopReason::Stalled,
                        Some("目标模式结束但未调用 update_goal(complete/blocked)".to_string()),
                    )
                } else {
                    (StopReason::Answered, None)
                };
                sync_epoch_progress(
                    &mut progress,
                    &mut metrics,
                    epoch_rounds_at_start,
                    epoch_tokens_at_start,
                    epoch_duration_at_start,
                    run_started,
                    round,
                    model_tokens_spent,
                    commands_run,
                    cost_spent_micros,
                    &modified_files,
                );
                observer(AgentEvent::ProgressUpdated {
                    ledger: progress.clone(),
                });

                return Ok(AgentOutcome {
                    final_text: last_text,
                    rounds: round,
                    modified_files,
                    stop_reason,
                    stop_detail,
                    metrics: metrics.clone(),
                    progress: progress.clone(),
                    objective: objective.clone(),
                });
            }

            // Tool results, filled by call index. Parallel-safe read-only tools
            // may finish out of order, but the model must receive their results
            // in the original call order — providers reject reordered results.
            let mut results: Vec<Option<ContentPart>> = (0..calls.len()).map(|_| None).collect();
            // Goal mode: set when the model calls update_goal this round; the run
            // ends (Completed/Blocked) once this round's tool results are recorded.
            let mut goal_resolution: Option<(StopReason, String)> = None;
            // Images loaded via view_image this round, injected as a user message
            // after the tool results so a vision model sees them next request.
            let mut pending_images: Vec<ContentPart> = Vec::new();
            // Read-only, side-effect-free tools deferred to run concurrently
            // after this in-order pass. Everything else runs here, serially.
            struct ParallelJob {
                index: usize,
                call: ToolCall,
                ctx: ToolContext,
                loop_key: String,
            }
            let mut parallel_jobs: Vec<ParallelJob> = Vec::new();
            // spawn_agent calls deferred to run concurrently after this pass.
            let mut spawn_jobs: Vec<(usize, ToolCall)> = Vec::new();
            // Pure-observe refused this round after a completed plan.
            let mut closeout_observe_denied_this_round = false;
            observe_only_tools_this_round = 0;
            non_observe_success_this_round = 0;

            for (index, call) in calls.into_iter().enumerate() {
                // Complex tasks must register a structured plan before mutation.
                // Read-only explore tools may run for PLAN_EXPLORE_ROUNDS first
                // (W2-04). Clarification and permission requests stay available.
                if structured_plan_required
                    && !structured_plan_started
                    && !matches!(
                        call.name.as_str(),
                        "update_plan"
                            | "request_user_input"
                            | "ask_user"
                            | REQUEST_PERMISSIONS_TOOL
                    )
                {
                    let explore_ok = is_plan_explore_tool(&call.name)
                        && plan_explore_rounds_used < PLAN_EXPLORE_ROUNDS;
                    if !explore_ok {
                        metrics.plan_first_write_blocked += 1;
                        let msg = if plan_explore_rounds_used < PLAN_EXPLORE_ROUNDS {
                            "This task has multiple independently verifiable steps. Call \
                             update_plan first with one in_progress step and the remaining \
                             steps pending; a prose checklist does not satisfy the plan gate. \
                             Read-only explore tools (read/grep/list/search) are allowed before the plan."
                                .to_string()
                        } else {
                            "Explore budget used. Call update_plan with one in_progress step \
                             and remaining steps pending before any further tools."
                                .to_string()
                        };
                        results[index] = Some(deny_call(observer, call, msg));
                        continue;
                    }
                }

                // Cap consecutive search calls so the model acts on
                // what they have instead of searching in circles (spec §17).
                if self.max_search_calls_per_step > 0 && is_search_tool(&call.name) {
                    consecutive_searches += 1;
                    if consecutive_searches > self.max_search_calls_per_step {
                        let msg = format!(
                            "Search budget reached ({} consecutive searches). Use the results you \
                             already have and take an action (read a specific file or edit) \
                             instead of searching again.",
                            self.max_search_calls_per_step
                        );
                        results[index] = Some(deny_call(observer, call, msg));
                        continue;
                    }
                }

                // Delivery: complete_step with evidence_ref against the ledger.
                if self.goal_mode && self.delivery_gate && call.name == COMPLETE_STEP_TOOL {
                    observer(AgentEvent::ToolCall {
                        id: call.id.as_str().to_string(),
                        name: COMPLETE_STEP_TOOL.to_string(),
                        arguments: compact_json(&call.arguments),
                    });
                    let step_id = call
                        .arguments
                        .get("step_id")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .trim()
                        .to_string();
                    let summary = call
                        .arguments
                        .get("summary")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .trim()
                        .to_string();
                    let evidence_ref = call
                        .arguments
                        .get("evidence_ref")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .trim()
                        .to_string();
                    let (ok, msg) = if step_id.is_empty() || evidence_ref.is_empty() {
                        (
                            false,
                            "complete_step requires step_id and evidence_ref".to_string(),
                        )
                    } else if !ledger.evidence_ref_is_fresh(&evidence_ref) {
                        (
                            false,
                            format!(
                                "evidence_ref `{evidence_ref}` is missing or stale after a later mutation; re-run verification"
                            ),
                        )
                    } else {
                        let step_text = plan_state
                            .steps
                            .iter()
                            .find(|s| {
                                s.id.as_deref() == Some(step_id.as_str()) || s.step == step_id
                            })
                            .map(|s| s.step.clone())
                            .unwrap_or_else(|| step_id.clone());
                        // Mark matching plan step completed in the mirror.
                        for s in &mut plan_state.steps {
                            if s.id.as_deref() == Some(step_id.as_str()) || s.step == step_id {
                                s.status = "completed".to_string();
                            }
                        }
                        ledger.plan = plan_state.clone();
                        ledger.record_step_receipt(CompleteStepReceipt {
                            step_id: step_id.clone(),
                            step_text: step_text.clone(),
                            summary: summary.clone(),
                            evidence_ref: evidence_ref.clone(),
                        });
                        metrics.plan_updated += 1;
                        observer(AgentEvent::PlanUpdated {
                            steps: plan_state.steps.clone(),
                        });
                        observer(AgentEvent::EvidenceLedgerUpdated {
                            ledger: ledger.clone(),
                        });
                        (
                            true,
                            format!("Step `{step_text}` completed with evidence `{evidence_ref}`."),
                        )
                    };
                    observer(AgentEvent::ToolResult {
                        id: call.id.as_str().to_string(),
                        name: COMPLETE_STEP_TOOL.to_string(),
                        is_error: !ok,
                        preview: preview(&msg),
                    });
                    results[index] = Some(ContentPart::ToolResult {
                        result: ToolResultContent {
                            call_id: call.id,
                            content: msg,
                            is_error: !ok,
                        },
                    });
                    continue;
                }

                // Goal mode: the model explicitly resolves the objective. Record
                // the resolution; the run ends after this round's results are
                // committed (so the transcript stays well-formed).
                if self.goal_mode && call.name == UPDATE_GOAL_TOOL {
                    // Surface the resolution so the TUI/JSONL shows the goal being
                    // closed (special tools otherwise skip the ToolCall event).
                    observer(AgentEvent::ToolCall {
                        id: call.id.as_str().to_string(),
                        name: UPDATE_GOAL_TOOL.to_string(),
                        arguments: compact_json(&call.arguments),
                    });
                    // A resolution without an explicit status is not accepted as
                    // completion — feed the error back so the model resolves
                    // deliberately instead of by omission.
                    let reason = match call.arguments.get("status").and_then(|v| v.as_str()) {
                        Some("complete") => StopReason::Completed,
                        Some("blocked") => StopReason::Blocked,
                        other => {
                            let feedback = format!(
                                "update_goal requires `status` set to \"complete\" or \
                                 \"blocked\" (got {}). Call update_goal again with an \
                                 explicit status.",
                                other
                                    .map(|s| format!("\"{s}\""))
                                    .unwrap_or_else(|| "no status".to_string())
                            );
                            observer(AgentEvent::ToolResult {
                                id: call.id.as_str().to_string(),
                                name: UPDATE_GOAL_TOOL.to_string(),
                                is_error: true,
                                preview: preview(&feedback),
                            });
                            results[index] = Some(ContentPart::ToolResult {
                                result: ToolResultContent {
                                    call_id: call.id,
                                    content: feedback,
                                    is_error: true,
                                },
                            });
                            continue;
                        }
                    };
                    // S2/S4 Gate: todos + (Delivery) EvidenceLedger.
                    if reason == StopReason::Completed {
                        let gate = GateConfig {
                            goal_todo_gate: self.goal_todo_gate,
                            todo_override_allowed: true,
                            delivery_gate: self.delivery_gate,
                            reject_unproven_no_mutation: self.delivery_gate,
                        };
                        ledger.plan = plan_state.clone();
                        // Explicit structured flag only — never attempt-count bypass.
                        let explicit_todo_override = call
                            .arguments
                            .get("override_incomplete_todos")
                            .and_then(|v| v.as_bool())
                            .unwrap_or(false);
                        if let Err(fail) = check(
                            &plan_state,
                            &ledger,
                            Some(&task_contract),
                            &gate,
                            explicit_todo_override,
                            task_looks_like_implementation(&original_task),
                        ) {
                            ledger.record_intercept("update_goal", fail.to_string());
                            ledger.plan = plan_state.clone();
                            observer(AgentEvent::GoalIntercepted {
                                kind: "update_goal".into(),
                                detail: fail.to_string(),
                            });
                            observer(AgentEvent::EvidenceLedgerUpdated {
                                ledger: ledger.clone(),
                            });
                            let feedback = format!(
                                "update_goal(complete) refused: {fail}. Finish remaining \
                                 plan steps and/or satisfy delivery evidence, then try again. \
                                 Incomplete todos require override_incomplete_todos=true \
                                 (only when override is allowed) — a second bare complete is not enough."
                            );
                            observer(AgentEvent::ToolResult {
                                id: call.id.as_str().to_string(),
                                name: UPDATE_GOAL_TOOL.to_string(),
                                is_error: true,
                                preview: preview(&feedback),
                            });
                            results[index] = Some(ContentPart::ToolResult {
                                result: ToolResultContent {
                                    call_id: call.id,
                                    content: feedback,
                                    is_error: true,
                                },
                            });
                            continue;
                        }
                        // HostImplicit single-step completes atomically with the goal.
                        if plan_state.is_host_implicit() {
                            plan_state.mark_all_completed();
                            metrics.plan_updated += 1;
                            observer(AgentEvent::PlanUpdated {
                                steps: plan_state.steps.clone(),
                            });
                        }
                    }
                    observer(AgentEvent::ToolResult {
                        id: call.id.as_str().to_string(),
                        name: UPDATE_GOAL_TOOL.to_string(),
                        is_error: false,
                        preview: "Goal resolved.".to_string(),
                    });
                    let summary = call
                        .arguments
                        .get("summary")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .trim()
                        .to_string();
                    goal_resolution = Some((reason, summary));
                    results[index] = Some(ContentPart::ToolResult {
                        result: ToolResultContent {
                            call_id: call.id,
                            content: "Goal resolved.".to_string(),
                            is_error: false,
                        },
                    });
                    continue;
                }

                // Clarification tools (request_user_input / ask_user) are answered
                // by the clarifier (the UI), not the tool registry (spec §35 / B7).
                if is_user_input_tool(&call.name) {
                    let answer = self.handle_ask_user(&call, &cancellation).await?;
                    results[index] = Some(ContentPart::ToolResult {
                        result: ToolResultContent {
                            call_id: call.id,
                            content: answer,
                            is_error: false,
                        },
                    });
                    continue;
                }

                // spawn_agent defers to the concurrent batch after this pass, so
                // several spawns in one round run in parallel.
                if call.name == SPAWN_AGENT_TOOL {
                    spawn_jobs.push((index, call));
                    continue;
                }

                // request_permissions is answered by the user, not the registry.
                if call.name == REQUEST_PERMISSIONS_TOOL {
                    let (granted, message, grants) = self
                        .handle_request_permissions(&call, &cancellation)
                        .await?;
                    if granted {
                        turn_grants = turn_grants.merge(grants);
                    }
                    results[index] = Some(ContentPart::ToolResult {
                        result: ToolResultContent {
                            call_id: call.id,
                            content: message,
                            is_error: !granted,
                        },
                    });
                    continue;
                }

                // A nested AGENTS.md may apply to a file the model tries to edit
                // directly. Refuse that first edit and inject the scoped rules
                // on the next round; otherwise the edit lands before the model
                // ever sees the rules governing it.
                if matches!(call.name.as_str(), "apply_patch" | "replace") {
                    let mut target_paths = scoped_paths.clone();
                    collect_scoped_paths_from_call(&call, &mut target_paths);
                    let fresh = load_scoped_rules(
                        self.tool_context.workspace.root(),
                        &target_paths,
                        &injected_rule_sources,
                    );
                    if !fresh.is_empty() {
                        scoped_paths = target_paths;
                        let sources = fresh
                            .iter()
                            .map(|rule| rule.source.as_str())
                            .collect::<Vec<_>>()
                            .join(", ");
                        let msg = format!(
                            "Edit paused before execution: project rules were discovered for \
                             this path ({sources}). They will be injected as system rules on \
                             the next model step; review them, then retry the edit."
                        );
                        results[index] = Some(deny_call(observer, call, msg));
                        continue;
                    }
                }

                // Closing phase: plan fully completed → refuse pure observe thrash.
                if plan_state.is_fully_completed() {
                    progress.enter_closing();
                }
                if progress.should_refuse_observe_in_closing()
                    && is_pure_observe_call(&call.name, &call.arguments)
                {
                    closeout_observe_denied_this_round = true;
                    observe_only_tools_this_round = observe_only_tools_this_round.saturating_add(1);
                    let msg = "Plan steps are complete. Do not re-check git status, \
                         re-list files, or re-audit prior questions — reply with a \
                         final summary only and stop calling tools."
                        .to_string();
                    results[index] = Some(deny_call(observer, call, msg));
                    continue;
                }
                if is_pure_observe_call(&call.name, &call.arguments) {
                    observe_only_tools_this_round = observe_only_tools_this_round.saturating_add(1);
                }

                // No-progress loop guard: same observe class (e.g. git status via
                // run_command vs shell_command) or exact (tool, args) already
                // produced an identical result LOOP_GUARD_THRESHOLD times.
                let loop_key = observe_class(&call.name, &call.arguments)
                    .unwrap_or_else(|| format!("{}\0{}", call.name, compact_json(&call.arguments)));
                if call_history.get(&loop_key).map(|(_, n)| *n).unwrap_or(0) >= LOOP_GUARD_THRESHOLD
                {
                    let msg = format!(
                        "This exact `{}` call already ran {} times with the same result and made \
                         no progress. Do something different — change the arguments or take \
                         another action.",
                        call.name, LOOP_GUARD_THRESHOLD
                    );
                    results[index] = Some(deny_call(observer, call, msg));
                    continue;
                }

                // Step budgets (spec §27): refuse the call BEFORE it runs once a
                // limit is reached; the run ends after this round's results are
                // committed. File budget also refuses a single multi-file patch
                // that would cross the remaining task-level cap (not only when
                // already exhausted).
                let epoch_file_count = projected_epoch_file_count(&progress, &modified_files);
                let over_budget = match call.name.as_str() {
                    // All shell paths count (including verify/acceptance-class runs).
                    // Some(0) = hard exhausted (not unlimited).
                    "run_command" | "shell_command"
                        if self
                            .step_limits
                            .max_commands
                            .is_some_and(|max| commands_run >= max) =>
                    {
                        Some(format!(
                            "the {}-command budget is exhausted",
                            self.step_limits.max_commands.unwrap_or(0)
                        ))
                    }
                    "apply_patch" | "replace" => file_budget_refusal(
                        self.step_limits.max_modified_files,
                        epoch_file_count,
                        &call,
                        &progress,
                        &modified_files,
                    ),
                    _ => None,
                };
                if let Some(which) = over_budget {
                    let msg = format!("Refused: {which}. The run stops here.");
                    budget_exceeded = Some(format!("Stopped: {which}."));
                    results[index] = Some(deny_call(observer, call, msg));
                    continue;
                }
                // Write allowlist (worker sub-agents, orchestrated nodes):
                // reject an edit that reaches outside the allowed paths BEFORE
                // it runs, feeding the reason back so the model stays in scope.
                if matches!(call.name.as_str(), "apply_patch" | "replace")
                    && let Some(allow) = &self.write_allowlist
                {
                    let outside = write_targets_outside_allowlist(&call, allow);
                    if !outside.is_empty() {
                        let msg = format!(
                            "Edit rejected: {} is outside your allowed paths ({}). Only edit \
                             within them; ask for the others if you truly need them.",
                            outside.join(", "),
                            allow.join(", ")
                        );
                        results[index] = Some(deny_call(observer, call, msg));
                        continue;
                    }
                }

                observer(AgentEvent::ToolCall {
                    id: call.id.as_str().to_string(),
                    name: call.name.clone(),
                    arguments: compact_json(&call.arguments),
                });
                collect_scoped_paths_from_call(&call, &mut scoped_paths);

                // Build the effective context: apply turn grants from
                // request_permissions (network and/or unrestricted FS).
                let ctx = apply_turn_grants(self.tool_context.clone(), turn_grants);
                // Full epoch path set so tools count "new" files correctly
                // (re-edits of already-budgeted paths do not consume residual).
                let epoch_paths = epoch_modified_paths(&progress, &modified_files);
                let remaining_files = self
                    .step_limits
                    .max_modified_files
                    .map(|max| max.saturating_sub(epoch_paths.len()));
                let ctx = ctx.with_command_write_constraints(
                    self.write_allowlist.clone(),
                    remaining_files,
                    epoch_paths,
                );

                // Read-only, side-effect-free tools are deferred to the
                // concurrent batch below; every other tool runs here, in order.
                let parallel = self
                    .registry
                    .get(&call.name)
                    .map(|t| t.supports_parallel())
                    .unwrap_or(false);
                let (content, is_error, image, workspace_snapshot, plan, newly_modified) =
                    match self
                        .authorize_with_cancellation(&call, &mut session_approved, &cancellation)
                        .await
                    {
                        Ok(()) if parallel => {
                            if matches!(call.name.as_str(), "run_command" | "shell_command") {
                                commands_run += 1;
                            }
                            // Host openers (`open`/`xdg-open`) only work outside
                            // seatbelt; elevate after authorize (user already OK'd).
                            let mut ctx = ctx;
                            if call_needs_host_escape(&call) {
                                ctx.turn_unrestricted_fs = true;
                            }
                            parallel_jobs.push(ParallelJob {
                                index,
                                call,
                                ctx,
                                loop_key,
                            });
                            continue;
                        }
                        Ok(()) => {
                            if matches!(call.name.as_str(), "run_command" | "shell_command") {
                                commands_run += 1;
                            }
                            let mut ctx = ctx;
                            if call_needs_host_escape(&call) {
                                ctx.turn_unrestricted_fs = true;
                            }
                            let files_before = modified_files.clone();
                            let (content, is_error, image, workspace_snapshot, plan) = self
                                .dispatch(&call, ctx, &mut modified_files, &cancellation)
                                .await;
                            let newly = newly_modified_paths(&files_before, &modified_files);
                            (content, is_error, image, workspace_snapshot, plan, newly)
                        }
                        Err(reason) => (
                            format!("action not permitted: {reason}"),
                            true,
                            None,
                            None,
                            None,
                            Vec::new(),
                        ),
                    };
                if let Some(snapshot) = workspace_snapshot {
                    observer(AgentEvent::WorkspaceSnapshot {
                        call_id: call.id.as_str().to_string(),
                        snapshot,
                    });
                }
                if !is_error {
                    progress_since_evidence_nudge = true;
                    if !is_pure_observe_call(&call.name, &call.arguments) {
                        non_observe_success_this_round =
                            non_observe_success_this_round.saturating_add(1);
                    }
                }
                if let Some(part) = image {
                    pending_images.push(part);
                }

                // Update the loop-guard window: identical output → count up,
                // any change (progress) → reset to this new result.
                match call_history.get_mut(&loop_key) {
                    Some(entry) if entry.0 == content => entry.1 += 1,
                    _ => {
                        call_history.insert(loop_key, (content.clone(), 1));
                    }
                }

                // Validate plan updates against the in-memory mirror before
                // accepting them (skip-step / origin rules). Host mirror only
                // advances on success; tool text is rewritten on rejection.
                let mut content = content;
                let mut is_error = is_error;
                if let Some(steps) = plan
                    && !is_error
                {
                    match PlanState::from_model_explicit(steps) {
                        Ok(next) => {
                            if let Err(msg) =
                                PlanState::validate_no_skip_complete(&plan_state, &next)
                            {
                                content = msg;
                                is_error = true;
                            } else {
                                plan_state = next;
                                structured_plan_started = true;
                                metrics.plan_updated += 1;
                                observer(AgentEvent::PlanUpdated {
                                    steps: plan_state.steps.clone(),
                                });
                            }
                        }
                        Err(msg) => {
                            content = msg;
                            is_error = true;
                        }
                    }
                }

                observer(AgentEvent::ToolResult {
                    id: call.id.as_str().to_string(),
                    name: call.name.clone(),
                    is_error,
                    preview: preview(&content),
                });

                // A concrete action clears the consecutive-search counter.
                if !is_search_tool(&call.name) {
                    consecutive_searches = 0;
                }
                // A passing verification-class command is completion evidence;
                // an arbitrary command (echo, ls, …) is not. shell_command never
                // counts (name gate inside counts_as_verification_evidence).
                if !is_error && counts_as_verification_evidence(&call.name, &call.arguments) {
                    verification_ran = true;
                    let (program, args) = extract_command(&call);
                    let fp = EvidenceLedger::normalize_command_fingerprint(
                        program.as_deref().unwrap_or("run_command"),
                        &args,
                    );
                    ledger.record_verify(call.id.as_str(), fp, 0);
                    ledger.plan = plan_state.clone();
                    observer(AgentEvent::EvidenceLedgerUpdated {
                        ledger: ledger.clone(),
                    });
                }
                // Any tool that newly modified files records a mutation (not
                // only apply_patch/replace by name). Paths are this call only.
                if !is_error && !newly_modified.is_empty() {
                    if matches!(call.name.as_str(), "apply_patch" | "replace") {
                        non_observe_success_this_round =
                            non_observe_success_this_round.saturating_add(1);
                    }
                    verification_ran = false;
                    note_tool_side_effects(
                        &mut ledger,
                        call.id.as_str(),
                        call.name.as_str(),
                        newly_modified,
                        &plan_state,
                        observer,
                    );
                    if matches!(call.name.as_str(), "apply_patch" | "replace")
                        && !structured_plan_started
                    {
                        metrics.first_write_before_plan = true;
                    }
                }
                for path in &modified_files {
                    push_unique_path(&mut scoped_paths, path);
                }

                results[index] = Some(ContentPart::ToolResult {
                    result: ToolResultContent {
                        call_id: call.id,
                        // Already size-capped centrally by `ToolRegistry::execute`.
                        content,
                        is_error,
                    },
                });
            }

            // Run the deferred read-only tools concurrently, then fold their
            // results back into their original call slots so the transcript
            // stays in call order regardless of completion order.
            if !parallel_jobs.is_empty() {
                use futures::stream::{FuturesUnordered, StreamExt};
                let cancellation_ref = &cancellation;
                // Leveling knob: bound how many of the batch actually overlap
                // (policy `max_parallel_tools`; 0 = the whole batch at once).
                let permits = match self.max_parallel_tools {
                    0 => parallel_jobs.len(),
                    n => n,
                };
                let sem = Arc::new(tokio::sync::Semaphore::new(permits.max(1)));
                let mut futs = FuturesUnordered::new();
                for job in &parallel_jobs {
                    let sem = sem.clone();
                    futs.push(async move {
                        let _permit = sem
                            .acquire()
                            .await
                            .expect("tool-batch semaphore is never closed");
                        let out = self
                            .dispatch_raw(&job.call, job.ctx.clone(), cancellation_ref)
                            .await;
                        (job.index, out)
                    });
                }
                let mut raw: std::collections::HashMap<usize, (String, bool, serde_json::Value)> =
                    std::collections::HashMap::new();
                while let Some((idx, out)) = futs.next().await {
                    raw.insert(idx, out);
                }
                drop(futs);

                for job in &parallel_jobs {
                    let (content, is_error, metadata) = raw
                        .remove(&job.index)
                        .expect("every parallel job produced a result");
                    let mut job_files = Vec::new();
                    collect_modified(&metadata, &mut job_files);
                    for f in &job_files {
                        if !modified_files.iter().any(|e| e == f) {
                            modified_files.push(f.clone());
                        }
                    }
                    if !is_error {
                        progress_since_evidence_nudge = true;
                        if !job_files.is_empty() {
                            verification_ran = false;
                            note_tool_side_effects(
                                &mut ledger,
                                job.call.id.as_str(),
                                job.call.name.as_str(),
                                job_files.clone(),
                                &plan_state,
                                observer,
                            );
                        }
                    }
                    if let Some(part) = extract_image(&metadata) {
                        pending_images.push(part);
                    }
                    match call_history.get_mut(&job.loop_key) {
                        Some(entry) if entry.0 == content => entry.1 += 1,
                        _ => {
                            call_history.insert(job.loop_key.clone(), (content.clone(), 1));
                        }
                    }
                    observer(AgentEvent::ToolResult {
                        id: job.call.id.as_str().to_string(),
                        name: job.call.name.clone(),
                        is_error,
                        preview: preview(&content),
                    });
                    if !is_error && let Some(steps) = extract_plan(&metadata) {
                        observer(AgentEvent::PlanUpdated { steps });
                    }
                    if !is_search_tool(&job.call.name) {
                        consecutive_searches = 0;
                    }
                    for path in &modified_files {
                        push_unique_path(&mut scoped_paths, path);
                    }
                    results[job.index] = Some(ContentPart::ToolResult {
                        result: ToolResultContent {
                            call_id: job.call.id.clone(),
                            // Already size-capped centrally by `ToolRegistry::execute`.
                            content,
                            is_error,
                        },
                    });
                }
            }

            // Concurrent sub-agent batch (CC-style star delegation): several
            // spawn_agent calls in one round run in parallel, bounded by
            // max_concurrent_agents. Each sub-agent's result folds into its call
            // slot; start/finish bubbles to the observer for the UI.
            if !spawn_jobs.is_empty() {
                use futures::stream::{FuturesUnordered, StreamExt};
                let sem = Arc::new(tokio::sync::Semaphore::new(
                    self.max_concurrent_agents.max(1),
                ));
                let (progress_tx, mut progress_rx) = tokio::sync::mpsc::unbounded_channel();
                let mut futs = FuturesUnordered::new();
                // Parent may have run shells/edits in this same tool batch
                // before children. Pin that spend on the ledger now — otherwise
                // absorb_child_spend + `commands_run = progress.cumulative_*`
                // overwrites local counters with a lagging ledger (mixed batch
                // under-counts parent commands).
                pin_parent_batch_spend(
                    &mut progress,
                    commands_run,
                    model_tokens_spent,
                    cost_spent_micros,
                    &modified_files,
                );
                // Pass 1: reject invalid spawns. Pass 2: split residual only
                // across *accepted* children (rejected slots must not dilute
                // the share — and must not let accepted children oversell).
                let mut accepted: Vec<(
                    usize,
                    leveler_core::ToolCallId,
                    AgentRole,
                    Vec<String>,
                    String,
                    String,
                    String,
                )> = Vec::new();
                for (index, call) in spawn_jobs {
                    let task = call
                        .arguments
                        .get("task")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .trim()
                        .to_string();
                    let role =
                        AgentRole::parse(call.arguments.get("role").and_then(|v| v.as_str()));
                    let files: Vec<String> = call
                        .arguments
                        .get("files")
                        .and_then(|v| v.as_array())
                        .map(|a| {
                            a.iter()
                                .filter_map(|x| x.as_str().map(String::from))
                                .collect()
                        })
                        .unwrap_or_default();

                    // Reject (no agent started) for depth, empty task, or cap.
                    let reject = if self.depth >= MAX_SUB_AGENT_DEPTH {
                        Some("Sub-agents may not spawn their own sub-agents.".to_string())
                    } else if task.is_empty() {
                        Some("spawn_agent requires a non-empty task.".to_string())
                    } else if agents_spawned >= self.max_total_agents {
                        Some(format!(
                            "Sub-agent limit reached ({} max this run). Do the remaining work \
                             directly.",
                            self.max_total_agents
                        ))
                    } else {
                        None
                    };
                    if let Some(msg) = reject {
                        observer(AgentEvent::ToolResult {
                            id: call.id.as_str().to_string(),
                            name: SPAWN_AGENT_TOOL.to_string(),
                            is_error: true,
                            preview: msg.clone(),
                        });
                        results[index] = Some(ContentPart::ToolResult {
                            result: ToolResultContent {
                                call_id: call.id,
                                content: msg,
                                is_error: true,
                            },
                        });
                        continue;
                    }

                    agents_spawned += 1;
                    let id = format!("agent-{agents_spawned}");
                    let nickname = agent_nickname(agents_spawned);
                    observer(AgentEvent::SubAgentStarted {
                        id: id.clone(),
                        nickname: nickname.clone(),
                        role: role.label().to_string(),
                        task: task.clone(),
                    });
                    accepted.push((index, call.id, role, files, task, id, nickname));
                }
                let share_n = accepted.len() as u32;
                for (share_of, (index, call_id, role, files, task, id, nickname)) in
                    accepted.into_iter().enumerate()
                {
                    let sem = sem.clone();
                    let progress_ch = progress_tx.clone();
                    let token = cancellation.child_token();
                    // Residual parent budgets split across concurrent spawns.
                    let residual = residual_step_limits(
                        self.step_limits,
                        commands_run,
                        model_tokens_spent,
                        cost_spent_micros,
                        projected_epoch_file_count(&progress, &modified_files),
                        epoch_duration_at_start,
                        run_started,
                        share_of as u32,
                        share_n,
                    );
                    let parent_wall = super::handlers::ParentWallBudget {
                        cap: self.step_limits.max_duration,
                        epoch_duration_at_start,
                        run_started,
                    };
                    futs.push(async move {
                        let result = self
                            .run_one_sub_agent(
                                id.clone(),
                                role,
                                files,
                                task,
                                sem,
                                progress_ch,
                                residual,
                                token,
                                parent_wall,
                            )
                            .await;
                        (index, call_id, id, nickname, result)
                    });
                }
                drop(progress_tx);

                while !futs.is_empty() {
                    tokio::select! {
                        biased;
                        Some(progress_ev) = progress_rx.recv() => observer(progress_ev),
                        Some((index, call_id, id, nickname, result)) = futs.next() => {
                            observer(AgentEvent::SubAgentFinished {
                                id,
                                nickname: nickname.clone(),
                                ok: result.ok,
                                summary: preview(&result.text),
                            });
                            if result.ok {
                                progress_since_evidence_nudge = true;
                            }
                            // Roll sub-agent spend into the parent task epoch.
                            // Parent same-batch spend was pinned before absorb.
                            progress.absorb_child_spend(&result.progress);
                            commands_run = progress.cumulative_commands;
                            model_tokens_spent = progress.cumulative_model_tokens;
                            cost_spent_micros = progress.cumulative_cost_usd_micros;
                            for path in result.modified_files {
                                if !modified_files.iter().any(|p| p == &path) {
                                    modified_files.push(path);
                                }
                            }
                            let content =
                                format!("[sub-agent {nickname} result]\n{}", result.text);
                            results[index] = Some(ContentPart::ToolResult {
                                result: ToolResultContent {
                                    call_id,
                                    // Sub-agent results bypass the registry, so apply
                                    // the central cap here.
                                    content: leveler_tools::registry::cap_output(&content),
                                    is_error: !result.ok,
                                },
                            });
                        }
                    }
                }
                while let Ok(progress) = progress_rx.try_recv() {
                    observer(progress);
                }
                drop(futs);
                // Always flush after sub-agent batch so absorbed spend is durable
                // even when the parent is about to cancel.
                sync_epoch_progress(
                    &mut progress,
                    &mut metrics,
                    epoch_rounds_at_start,
                    epoch_tokens_at_start,
                    epoch_duration_at_start,
                    run_started,
                    round,
                    model_tokens_spent,
                    commands_run,
                    cost_spent_micros,
                    &modified_files,
                );
                observer(AgentEvent::ProgressUpdated {
                    ledger: progress.clone(),
                });
                if cancellation.is_cancelled() && !deadline_expired.load(Ordering::Acquire) {
                    return Err(AgentError::Cancelled);
                }
            }

            let results: Vec<ContentPart> = results
                .into_iter()
                .map(|r| r.expect("every tool call produced a result"))
                .collect();

            let tool_message = Message {
                role: Role::Tool,
                content: results,
            };
            messages.push(tool_message.clone());
            // Flush spend BEFORE transcript persistence: tools already ran, so
            // a sink I/O failure must not drop this batch's command/file totals.
            sync_epoch_progress(
                &mut progress,
                &mut metrics,
                epoch_rounds_at_start,
                epoch_tokens_at_start,
                epoch_duration_at_start,
                run_started,
                round,
                model_tokens_spent,
                commands_run,
                cost_spent_micros,
                &modified_files,
            );
            observer(AgentEvent::ProgressUpdated {
                ledger: progress.clone(),
            });
            sink.append(&[assistant, tool_message]).await?;

            // Progress assess: closeout thrash + pure-observe no-progress streaks.
            if plan_state.is_fully_completed() {
                progress.enter_closing();
            }
            let pure_observe_round = closeout_observe_denied_this_round
                || (observe_only_tools_this_round > 0
                    && non_observe_success_this_round == 0
                    && !verification_ran);
            if closeout_observe_denied_this_round {
                progress.note_closeout_deny_round();
                observer(AgentEvent::ProgressUpdated {
                    ledger: progress.clone(),
                });
                if progress.should_hard_stop_closeout(progress_caps) {
                    progress.enter_terminal();
                    observer(AgentEvent::ProgressUpdated {
                        ledger: progress.clone(),
                    });
                    let final_text = if last_text.trim().is_empty() {
                        "Stopped: plan complete; refused further observe-only thrash.".to_string()
                    } else {
                        last_text.clone()
                    };
                    observer(AgentEvent::Finished(final_text.clone()));
                    sync_epoch_progress(
                        &mut progress,
                        &mut metrics,
                        epoch_rounds_at_start,
                        epoch_tokens_at_start,
                        epoch_duration_at_start,
                        run_started,
                        round,
                        model_tokens_spent,
                        commands_run,
                        cost_spent_micros,
                        &modified_files,
                    );
                    observer(AgentEvent::ProgressUpdated {
                        ledger: progress.clone(),
                    });

                    return Ok(AgentOutcome {
                        final_text,
                        rounds: round,
                        modified_files,
                        // Not Answered/Completed — thrash is incomplete progress.
                        stop_reason: StopReason::Incomplete,
                        stop_detail: Some(
                            "plan complete; observe thrash short-circuited".to_string(),
                        ),
                        metrics: metrics.clone(),
                        progress: progress.clone(),
                        objective: objective.clone(),
                    });
                }
                if !post_plan_closeout_nudged {
                    post_plan_closeout_nudged = true;
                    messages.push(Message::text(
                        Role::User,
                        "Plan steps are complete. Your next message must be the final                          summary only — do not call tools, do not re-run git status, and                          do not reopen earlier questions."
                            .to_string(),
                    ));
                }
            } else if pure_observe_round {
                // Only count as thrash when observe output is *repeated*
                // (same content fingerprint). Fresh greps with new hits explore.
                // After ProgressCaps::no_progress_rounds of identical thrash,
                // hard-stop the turn (AC3) so UntilTerminal cannot spin forever.
                let identical_observe_thrash = call_history
                    .values()
                    .any(|(_, n)| *n >= LOOP_GUARD_THRESHOLD);
                if identical_observe_thrash {
                    progress.note_no_progress_round(round);
                    observer(AgentEvent::ProgressUpdated {
                        ledger: progress.clone(),
                    });
                    if !no_progress_nudge_sent {
                        no_progress_nudge_sent = true;
                        messages.push(Message::text(
                            Role::User,
                            format!(
                                "You are not making progress toward the active objective:\n\
                                 <objective>\n{original_task}\n</objective>\n\
                                 Stop re-listing/re-statusing. Either make a concrete change \
                                 (edit/verify) or give the final answer and stop calling tools."
                            ),
                        ));
                    }
                    // Sub-agents may legitimately re-list; only top-level turns
                    // full-stop on observe thrash (AC3).
                    if self.depth == 0 && progress.should_hard_stop_no_progress(progress_caps) {
                        progress.enter_terminal();
                        observer(AgentEvent::ProgressUpdated {
                            ledger: progress.clone(),
                        });
                        let final_text = if last_text.trim().is_empty() {
                            "Stopped: no progress (observe-only thrash).".to_string()
                        } else {
                            last_text.clone()
                        };
                        observer(AgentEvent::Finished(final_text.clone()));
                        sync_epoch_progress(
                            &mut progress,
                            &mut metrics,
                            epoch_rounds_at_start,
                            epoch_tokens_at_start,
                            epoch_duration_at_start,
                            run_started,
                            round,
                            model_tokens_spent,
                            commands_run,
                            cost_spent_micros,
                            &modified_files,
                        );
                        observer(AgentEvent::ProgressUpdated {
                            ledger: progress.clone(),
                        });

                        return Ok(AgentOutcome {
                            final_text,
                            rounds: round,
                            modified_files,
                            // Not Answered/Completed — thrash is incomplete progress.
                            stop_reason: StopReason::Incomplete,
                            stop_detail: Some(
                                "no-progress streak; observe thrash short-circuited".to_string(),
                            ),
                            metrics: metrics.clone(),
                            progress: progress.clone(),
                            objective: objective.clone(),
                        });
                    }
                }
            } else {
                // Successful non-observe work resets the streak.
                progress.note_progress(round);
            }
            // Per-round counters are zeroed at the top of the next model round.

            // Count a spent explore round when complex tasks still lack a plan.
            if structured_plan_required && !structured_plan_started {
                plan_explore_rounds_used = plan_explore_rounds_used
                    .saturating_add(1)
                    .min(PLAN_EXPLORE_ROUNDS);
            }

            // Goal mode: an explicit update_goal this round ends the run now that
            // its result is committed.
            if let Some((reason, summary)) = goal_resolution {
                let final_text = if summary.is_empty() {
                    last_text.clone()
                } else {
                    summary
                };
                // Epoch terminal: next Content turn must not inherit Closing state.
                if matches!(reason, StopReason::Completed | StopReason::Blocked) {
                    progress.enter_terminal();
                    observer(AgentEvent::ProgressUpdated {
                        ledger: progress.clone(),
                    });
                }
                observer(AgentEvent::Finished(final_text.clone()));
                sync_epoch_progress(
                    &mut progress,
                    &mut metrics,
                    epoch_rounds_at_start,
                    epoch_tokens_at_start,
                    epoch_duration_at_start,
                    run_started,
                    round,
                    model_tokens_spent,
                    commands_run,
                    cost_spent_micros,
                    &modified_files,
                );
                observer(AgentEvent::ProgressUpdated {
                    ledger: progress.clone(),
                });

                return Ok(AgentOutcome {
                    final_text,
                    rounds: round,
                    modified_files,
                    stop_reason: reason,
                    stop_detail: None,
                    metrics: metrics.clone(),
                    progress: progress.clone(),
                    objective: objective.clone(),
                });
            }

            // A step limit tripped this round: results are committed, stop now.
            if let Some(reason) = budget_exceeded {
                observer(AgentEvent::Finished(reason.clone()));
                sync_epoch_progress(
                    &mut progress,
                    &mut metrics,
                    epoch_rounds_at_start,
                    epoch_tokens_at_start,
                    epoch_duration_at_start,
                    run_started,
                    round,
                    model_tokens_spent,
                    commands_run,
                    cost_spent_micros,
                    &modified_files,
                );
                observer(AgentEvent::ProgressUpdated {
                    ledger: progress.clone(),
                });

                return Ok(AgentOutcome {
                    final_text: reason,
                    rounds: round,
                    modified_files,
                    stop_reason: StopReason::BudgetExhausted,
                    stop_detail: None,
                    metrics: metrics.clone(),
                    progress: progress.clone(),
                    objective: objective.clone(),
                });
            }

            // Surface any images loaded this round to the model (image content
            // parts live in a user message, not a tool result, for OpenAI-style
            // providers).
            if !pending_images.is_empty() {
                let image_message = Message {
                    role: Role::User,
                    content: pending_images,
                };
                messages.push(image_message.clone());
                sink.append(&[image_message]).await?;
            }

            // Auto-compaction (spec §53): when the context size exceeds the
            // budget, fold the in-memory transcript before the next request so a
            // long task never overflows the window. Prefer the provider's
            // reported token count, but fall back to a char/4 estimate — many
            // gateways don't report streaming usage, and without a fallback
            // compaction would silently never fire. The persisted transcript
            // (sink) is untouched — only what we resend shrinks.
            let context_tokens = used_tokens.max(estimate_tokens(&messages));
            if self.context_budget > 0
                && context_tokens > self.context_budget as u64
                && has_next_round
            {
                let before = messages.len();
                let summary = self
                    .summarize_for_compaction(&messages, COMPACT_KEEP_RECENT, &cancellation)
                    .await;
                messages = compact_messages(
                    &messages,
                    COMPACT_KEEP_RECENT,
                    summary.as_deref(),
                    Some(objective.text()),
                );
                if messages.len() < before {
                    observer(AgentEvent::Compacted {
                        from: before,
                        to: messages.len(),
                    });
                }
            }

            // Leveling: periodically make the model checkpoint its progress so a
            // long task does not drift (spec §17). Not persisted — a transient
            // nudge that shapes the next round only.
            if self.step_summary_every > 0
                && round.is_multiple_of(self.step_summary_every)
                && has_next_round
            {
                messages.push(Message {
                    role: Role::User,
                    content: vec![ContentPart::Text {
                        text: STEP_SUMMARY_NUDGE.to_string(),
                    }],
                });
            }

            // Persist the exact next-request context through the engine event
            // log. This includes compaction breadcrumbs and transient execution
            // nudges that do not exist in the append-only raw transcript.
            if has_next_round {
                observer(AgentEvent::ContextSnapshot {
                    messages: messages.clone(),
                });
            }
        }

        let round_limit = self
            .continuation
            .round_limit()
            .expect("only bounded continuation exits the loop by round count");
        // Budget exhausted: never return an empty answer. Surface the last thing
        // the model said plus how far it got, so the caller/UI shows real state.
        let summary = {
            let mut s = format!("Reached the {round_limit}-round limit before finishing.");
            if !modified_files.is_empty() {
                s.push_str(&format!(
                    " Files changed so far: {}.",
                    modified_files.join(", ")
                ));
            }
            if !last_text.trim().is_empty() {
                s.push_str(&format!("\n\nLatest note: {}", last_text.trim()));
            }
            s
        };
        sync_epoch_progress(
            &mut progress,
            &mut metrics,
            epoch_rounds_at_start,
            epoch_tokens_at_start,
            epoch_duration_at_start,
            run_started,
            round_limit,
            model_tokens_spent,
            commands_run,
            cost_spent_micros,
            &modified_files,
        );
        Ok(AgentOutcome {
            final_text: summary,
            rounds: round_limit,
            modified_files,
            stop_reason: StopReason::BudgetExhausted,
            stop_detail: None,
            metrics: metrics.clone(),
            progress: progress.clone(),
            objective: objective.clone(),
        })
    }
}

/// Pin parent-local batch spend onto the ledger before child absorb.
///
/// Local `commands_run` / tokens / cost advance when parent tools run in the
/// same assistant batch as `spawn_agent`; the ledger may still lag until the
/// next `sync_epoch_progress`. Without this pin, `absorb_child_spend` +
/// reassignment from `progress.cumulative_*` drops the parent's same-batch spend.
fn pin_parent_batch_spend(
    progress: &mut leveler_lifecycle::ProgressLedger,
    commands_run: u32,
    model_tokens_spent: u64,
    cost_spent_micros: u64,
    modified_files: &[String],
) {
    progress.cumulative_commands = progress.cumulative_commands.max(commands_run);
    progress.cumulative_model_tokens = progress.cumulative_model_tokens.max(model_tokens_spent);
    progress.cumulative_cost_usd_micros =
        progress.cumulative_cost_usd_micros.max(cost_spent_micros);
    progress.merge_modified_paths(modified_files.iter().cloned());
}

/// Residual step limits for one child. When the parent is capped, residual is
/// always `Some(_)` including `Some(0)` (hard block) — never re-opens unlimited.
///
/// `share_of` is the 0-based index of this child among `share_n` concurrent
/// spawns so the residual is split (no parallel oversell).
///
/// **Duration note:** wall residual is computed at queue time and refreshed
/// again after the concurrency semaphore is acquired (see `run_one_sub_agent`)
/// so a child that waited behind others cannot keep a pre-queue residual that
/// already exceeds the parent deadline.
fn residual_step_limits(
    parent: super::StepLimits,
    commands_run: u32,
    model_tokens_spent: u64,
    cost_spent_micros: u64,
    epoch_files: usize,
    epoch_duration_at_start: std::time::Duration,
    run_started: std::time::Instant,
    share_of: u32,
    share_n: u32,
) -> super::StepLimits {
    use super::StepLimits;
    let n = share_n.max(1);
    let idx = share_of.min(n - 1);
    let split = |total: u32| -> u32 {
        let base = total / n;
        let rem = total % n;
        base + u32::from(idx < rem)
    };
    let split_usize = |total: usize| -> usize {
        let n = n as usize;
        let idx = idx as usize;
        let base = total / n;
        let rem = total % n;
        base + usize::from(idx < rem)
    };
    let split_u64 = |total: u64| -> u64 {
        let n = u64::from(n);
        let idx = u64::from(idx);
        let base = total / n;
        let rem = total % n;
        base + u64::from(idx < rem)
    };

    let mut limits = StepLimits {
        max_duration: Some(crate::sub_agent::SUB_AGENT_MAX_DURATION),
        ..StepLimits::default()
    };
    if let Some(max) = parent.max_commands {
        // Some(0) when exhausted — child cannot run any command.
        let remaining = max.saturating_sub(commands_run);
        limits.max_commands = Some(split(remaining));
    }
    if let Some(max) = parent.max_model_tokens {
        let remaining = max.saturating_sub(model_tokens_spent);
        limits.max_model_tokens = Some(split_u64(remaining));
    }
    if let Some(max) = parent.max_cost_usd_micros {
        let remaining = max.saturating_sub(cost_spent_micros);
        limits.max_cost_usd_micros = Some(split_u64(remaining));
    }
    if let Some(max) = parent.max_modified_files {
        let remaining = max.saturating_sub(epoch_files);
        limits.max_modified_files = Some(split_usize(remaining));
    }
    // Child wall clock: min(sub-agent cap, parent residual). Exhausted parent
    // duration → Some(0) hard stop (not a free 1s grant).
    if let Some(parent_max) = parent.max_duration {
        let elapsed = epoch_duration_at_start.saturating_add(run_started.elapsed());
        let residual = parent_max.saturating_sub(elapsed);
        let child_cap = limits
            .max_duration
            .unwrap_or(crate::sub_agent::SUB_AGENT_MAX_DURATION);
        limits.max_duration = Some(child_cap.min(residual));
    }
    limits
}

/// Distinct file count if `drive_files` were merged into the epoch path set.
fn projected_epoch_file_count(
    progress: &leveler_lifecycle::ProgressLedger,
    drive_files: &[String],
) -> usize {
    epoch_modified_paths(progress, drive_files).len()
}

/// Epoch + this-drive distinct modified paths (source of truth for residual).
fn epoch_modified_paths(
    progress: &leveler_lifecycle::ProgressLedger,
    drive_files: &[String],
) -> Vec<String> {
    let mut paths = progress.cumulative_modified_paths.clone();
    for path in drive_files {
        if !paths.iter().any(|p| p == path) {
            paths.push(path.clone());
        }
    }
    paths
}

/// Task-level file budget gate for `apply_patch` / `replace`.
///
/// Refuses when this call would introduce more *new* paths than residual
/// allows (including multi-file patch oversell). Re-edits of paths already in
/// the epoch set are free and allowed even when residual is 0.
fn file_budget_refusal(
    max_modified_files: Option<usize>,
    epoch_file_count: usize,
    call: &ToolCall,
    progress: &leveler_lifecycle::ProgressLedger,
    drive_files: &[String],
) -> Option<String> {
    let max = max_modified_files?;
    let mut targets = Vec::new();
    collect_scoped_paths_from_call(call, &mut targets);
    let new_n = targets
        .iter()
        .filter(|path| {
            !progress
                .cumulative_modified_paths
                .iter()
                .any(|p| p == *path)
                && !drive_files.iter().any(|p| p == *path)
        })
        .count();
    if new_n == 0 {
        // Pure re-edit (or no paths parsed): does not grow the epoch set.
        return None;
    }
    let remaining = max.saturating_sub(epoch_file_count);
    if remaining == 0 {
        return Some(format!("the {max}-modified-file budget is exhausted"));
    }
    if new_n > remaining {
        return Some(format!(
            "this edit would modify {new_n} new file(s) but only {remaining} remain in the \
             {max}-modified-file budget"
        ));
    }
    None
}

/// Write absolute epoch spend into the ledger so continue/resume seeds the
/// same totals (including tool-phase command/file increments after the last
/// model stream).
#[allow(clippy::too_many_arguments)]
fn sync_epoch_progress(
    progress: &mut leveler_lifecycle::ProgressLedger,
    metrics: &mut DepthUseMetrics,
    epoch_rounds_at_start: u32,
    epoch_tokens_at_start: u64,
    epoch_duration_at_start: std::time::Duration,
    run_started: std::time::Instant,
    round: u32,
    model_tokens_spent: u64,
    commands_run: u32,
    cost_spent_micros: u64,
    modified_files: &[String],
) {
    metrics.model_tokens = model_tokens_spent.saturating_sub(epoch_tokens_at_start);
    let duration_ms = epoch_duration_at_start
        .saturating_add(run_started.elapsed())
        .as_millis() as u64;
    // Distinct paths across the epoch (re-edits of the same file do not inflate).
    progress.merge_modified_paths(modified_files.iter().cloned());
    let files_total = progress.cumulative_modified_files;
    progress.set_epoch_spend(
        epoch_rounds_at_start.saturating_add(round),
        model_tokens_spent,
        commands_run,
        cost_spent_micros,
        duration_ms,
        files_total,
    );
}

#[cfg(test)]
mod residual_budget_tests {
    use super::*;
    use crate::executor::StepLimits;
    use std::time::{Duration, Instant};

    #[test]
    fn exhausted_parent_commands_yield_some_zero_not_unlimited() {
        let parent = StepLimits {
            max_commands: Some(3),
            ..StepLimits::default()
        };
        let residual = residual_step_limits(
            parent,
            3, // already used all
            0,
            0,
            0,
            Duration::ZERO,
            Instant::now(),
            0,
            1,
        );
        assert_eq!(residual.max_commands, Some(0));
    }

    #[test]
    fn parallel_share_splits_without_oversell() {
        let parent = StepLimits {
            max_commands: Some(3),
            max_modified_files: Some(2),
            ..StepLimits::default()
        };
        let a = residual_step_limits(parent, 0, 0, 0, 0, Duration::ZERO, Instant::now(), 0, 2);
        let b = residual_step_limits(parent, 0, 0, 0, 0, Duration::ZERO, Instant::now(), 1, 2);
        // 3 commands / 2 children → 2 + 1
        assert_eq!(a.max_commands.unwrap() + b.max_commands.unwrap(), 3);
        // 2 files / 2 → 1 + 1
        assert_eq!(
            a.max_modified_files.unwrap() + b.max_modified_files.unwrap(),
            2
        );
    }

    #[test]
    fn exhausted_parent_duration_is_zero_not_one_second_grant() {
        let parent = StepLimits {
            max_duration: Some(Duration::from_secs(5)),
            ..StepLimits::default()
        };
        // Simulate parent already past the cap.
        let residual = residual_step_limits(
            parent,
            0,
            0,
            0,
            0,
            Duration::from_secs(10),
            Instant::now(),
            0,
            1,
        );
        assert_eq!(residual.max_duration, Some(Duration::ZERO));
    }

    #[test]
    fn unlimited_parent_stays_unlimited_for_child() {
        let parent = StepLimits::default();
        let residual =
            residual_step_limits(parent, 99, 0, 0, 0, Duration::ZERO, Instant::now(), 0, 1);
        assert_eq!(residual.max_commands, None);
        assert_eq!(residual.max_modified_files, None);
    }

    #[test]
    fn share_n_must_use_accepted_count_not_raw_job_count() {
        // Two accepted children of a 2-command residual: 1+1, not 2/3 + 0.
        let parent = StepLimits {
            max_commands: Some(2),
            ..StepLimits::default()
        };
        let a = residual_step_limits(parent, 0, 0, 0, 0, Duration::ZERO, Instant::now(), 0, 2);
        let b = residual_step_limits(parent, 0, 0, 0, 0, Duration::ZERO, Instant::now(), 1, 2);
        assert_eq!(a.max_commands, Some(1));
        assert_eq!(b.max_commands, Some(1));
        // If share_n wrongly included a rejected third job, each would get 0 or 1
        // with sum still 2 but uneven waste — sum of accepted shares must equal residual.
        assert_eq!(
            a.max_commands.unwrap() + b.max_commands.unwrap(),
            2,
            "accepted shares must exhaust residual without oversell"
        );
    }

    #[test]
    fn pin_parent_batch_spend_keeps_local_commands_before_absorb() {
        use leveler_lifecycle::ProgressLedger;
        // Ledger lags (0); local batch already spent 1 command.
        let mut progress = ProgressLedger {
            cumulative_commands: 0,
            ..Default::default()
        };
        pin_parent_batch_spend(&mut progress, 1, 0, 0, &[]);
        assert_eq!(progress.cumulative_commands, 1);
        let child = ProgressLedger {
            cumulative_commands: 1,
            ..Default::default()
        };
        progress.absorb_child_spend(&child);
        assert_eq!(
            progress.cumulative_commands, 2,
            "parent same-batch + child must both count"
        );
    }

    #[test]
    fn multi_file_patch_refused_when_residual_file_budget_is_one() {
        use leveler_core::ToolCallId;
        use leveler_lifecycle::ProgressLedger;
        use leveler_model::ToolCall;

        let progress = ProgressLedger::default();
        let call = ToolCall {
            id: ToolCallId::new("c"),
            name: "apply_patch".into(),
            arguments: serde_json::json!({
                "patch": "*** Begin Patch\n*** Update File: a.rs\n*** Update File: b.rs\n*** End Patch"
            }),
        };
        let reason = file_budget_refusal(Some(1), 0, &call, &progress, &[]);
        assert!(
            reason
                .as_deref()
                .is_some_and(|r| r.contains("2 new file") && r.contains("only 1 remain")),
            "expected multi-file residual refusal, got {reason:?}"
        );
        // Re-edit of an already-budgeted path does not consume residual.
        let mut with_a = ProgressLedger::default();
        with_a.merge_modified_paths(["a.rs"]);
        let reedit = ToolCall {
            id: ToolCallId::new("c2"),
            name: "apply_patch".into(),
            arguments: serde_json::json!({
                "patch": "*** Begin Patch\n*** Update File: a.rs\n*** End Patch"
            }),
        };
        assert_eq!(
            file_budget_refusal(Some(1), 1, &reedit, &with_a, &[]),
            None,
            "re-edit of counted path must be allowed at residual 0 new"
        );
    }
}
