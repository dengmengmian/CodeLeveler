use leveler_client_protocol::{
    NotificationLevel, PermissionProfile, RuntimeEvent, RuntimeStatus, UiRole, UiSessionSnapshot,
};

use crate::overlay::Overlay;
use crate::overlay::approval::ApprovalOverlay;
use crate::overlay::clarification::ClarificationOverlay;
use crate::state::{AppState, Notification, PendingInteraction};
use crate::transcript::TurnEndStatus;

pub(super) fn apply_runtime(state: &mut AppState, event: RuntimeEvent) {
    match event {
        RuntimeEvent::RuntimeReady => {}
        RuntimeEvent::SessionOpened { session } => apply_session(state, session),
        RuntimeEvent::SessionUpdated { session } => apply_meta(state, &session),
        RuntimeEvent::ApprovalRequested { request } => {
            // If any overlay is already open (an earlier unanswered approval or a
            // picker the user opened), park this request instead of clobbering the
            // active overlay — otherwise the earlier request would never be answered.
            if state.overlay.is_some() {
                state
                    .pending_interactions
                    .push_back(PendingInteraction::Approval(request));
            } else {
                state.overlay = Some(Overlay::Approval(Box::new(ApprovalOverlay::new(request))));
            }
        }
        RuntimeEvent::ClarificationRequested { request } => {
            // Same parking rule as approvals: a clobbered approval overlay would
            // leave its runtime-side oneshot unanswered and hang that tool call.
            if state.overlay.is_some() {
                state
                    .pending_interactions
                    .push_back(PendingInteraction::Clarification(request));
            } else {
                state.overlay = Some(Overlay::Clarification(Box::new(ClarificationOverlay::new(
                    request,
                ))));
            }
        }
        // Another client (or a timeout/cancel) answered this request. Dismiss the
        // matching prompt here so a second answer can't hit an already-resolved
        // approval. Harmless when this client is the one that just answered.
        RuntimeEvent::ApprovalResolved { id } => {
            dismiss_resolved_interaction(
                state,
                |p| matches!(p, PendingInteraction::Approval(r) if r.id == id),
            );
            if matches!(&state.overlay, Some(Overlay::Approval(ov)) if ov.request.id == id) {
                state.overlay = None;
                crate::reducer::overlay_keys::advance_overlay(state);
            }
        }
        RuntimeEvent::ClarificationResolved { id } => {
            dismiss_resolved_interaction(
                state,
                |p| matches!(p, PendingInteraction::Clarification(r) if r.id == id),
            );
            if matches!(&state.overlay, Some(Overlay::Clarification(ov)) if ov.request.id == id) {
                state.overlay = None;
                crate::reducer::overlay_keys::advance_overlay(state);
            }
        }
        RuntimeEvent::AttachmentAdded { attachment } => {
            state.pending_attachments.push(attachment);
        }
        RuntimeEvent::AttachmentProcessingFailed { error } => {
            state.notification = Some(Notification {
                level: NotificationLevel::Error,
                message: format!("附件处理失败: {error}"),
            });
        }
        RuntimeEvent::UserMessageAdded { message } => {
            state.transcript.push_user_if_new(message.text);
        }
        RuntimeEvent::AssistantMessageStarted { message_id } => {
            mark_turn_busy(state);
            seal_analysis_segment(state);
            state.transcript.begin_assistant(message_id);
        }
        RuntimeEvent::AssistantAttemptReset { message_id } => {
            if let Some(message_id) = message_id {
                state.transcript.reset_assistant_attempt(&message_id);
            }
            seal_analysis_segment(state);
        }
        RuntimeEvent::AssistantTextDelta { message_id, delta } => {
            mark_turn_busy(state);
            state.transcript.append_assistant(&message_id, &delta);
        }
        RuntimeEvent::ReasoningDelta { delta } => {
            mark_turn_busy(state);
            // Raw reasoning never enters the transcript: it only keeps the
            // status line's thinking indicator honest while the model works.
            state.live_reasoning.push_str(&delta);
        }
        RuntimeEvent::AssistantMessageCompleted { message_id } => {
            state.transcript.finish_assistant(&message_id);
        }
        RuntimeEvent::AgentActivity { label } => {
            mark_turn_busy(state);
            state.activity = Some(label);
        }
        RuntimeEvent::CommandProgress { label, elapsed_ms } => {
            // Long-command heartbeat: name the running command with a live
            // elapsed so the status line reads "运行 cargo test · 02:31" instead
            // of a bare "等待模型". Reuses the activity slot (single source).
            mark_turn_busy(state);
            let secs = elapsed_ms / 1000;
            state.activity = Some(format!(
                "运行 {label} · {}",
                crate::status_line::fmt_elapsed(secs)
            ));
        }
        RuntimeEvent::ProjectRulesLoaded { sources } => {
            mark_turn_busy(state);
            state.project_rule_sources = sources;
        }
        RuntimeEvent::ToolCallStarted {
            id,
            name,
            arguments,
            parallel,
        } => {
            mark_turn_busy(state);
            state.turn_tool_calls = state.turn_tool_calls.saturating_add(1);
            // Acting on the thought ends the reasoning segment: the status
            // scratch is spent, and nothing else ever held that text.
            state.live_reasoning.clear();
            // Status line shows what the tool is DOING ("运行 cargo check -p x"),
            // not the internal tool name ("运行 run_command").
            let verb = crate::tool_taxonomy::presentation_label(&name, state.locale);
            let target = crate::render::tool_summary(&name, &arguments);
            state.activity = Some(if target.is_empty() || target == "{}" {
                verb
            } else {
                format!("{verb} {target}")
            });
            let started = state.elapsed_secs;
            state
                .transcript
                .push_tool_started(id, name, arguments, parallel, started);
        }
        RuntimeEvent::ToolCallCompleted {
            id,
            ok,
            preview,
            duration_ms,
        } => {
            // Strip ANSI and controls so vitest/npm color codes do not show as
            // `[32m` garbage when ESC was already dropped (cell TUI).
            let preview = leveler_core::sanitize_terminal_output(&preview);
            state
                .transcript
                .complete_tool(&id, ok, preview, duration_ms);
            // The tool is done; leaving its label up while the model thinks
            // reads as a hung tool ("读取 x… (4m)"). Fall back to the
            // thinking indicator until the next activity arrives.
            state.activity = None;
            seal_analysis_segment(state);
        }
        RuntimeEvent::PlanUpdated { plan } => {
            // Fully succeeded plans (incl. 1/1) clear immediately so the chrome
            // does not linger after the last ✓; open/failed plans stay.
            if crate::workbench::plan_panel_should_show(&plan) {
                state.plan = Some(plan);
            } else {
                state.plan = None;
            }
        }
        RuntimeEvent::VerificationUpdated { verification } => {
            state.verification = Some(verification);
        }
        RuntimeEvent::DiffUpdated { diff } => {
            if state.diff_selected >= diff.files.len() {
                state.diff_selected = 0;
            }
            state.diff = Some(diff);
        }
        RuntimeEvent::SessionCompleted { report } => {
            state.transcript.push_completion(report);
        }
        RuntimeEvent::CheckpointCreated { checkpoint } => {
            // Dedup by id — a replayed/lagged event must not add a duplicate.
            if !state.checkpoints.iter().any(|c| c.id == checkpoint.id) {
                state.checkpoints.push(checkpoint);
            }
        }
        RuntimeEvent::SessionList { sessions } => {
            if state.sessions_selected >= sessions.len() {
                state.sessions_selected = 0;
            }
            state.sessions = sessions;
        }
        RuntimeEvent::ContextUpdated {
            candidate_files,
            estimated_tokens,
        } => {
            state.context_files = candidate_files;
            // `estimated_tokens` is a pre-run guess (candidate files). Only use it
            // as a placeholder until the model reports real usage — don't let it
            // clobber a live TokenUsage reading and make the gauge jitter.
            if state.token_input == 0 && state.token_output == 0 {
                state.context_tokens = estimated_tokens;
            }
        }
        RuntimeEvent::TokenUsage {
            input_tokens,
            output_tokens,
            cached_input_tokens,
        } => {
            // Ignore all-zero reports so a missing provider usage chunk cannot
            // wipe a previous good reading (or a transcript estimate).
            if input_tokens == 0 && output_tokens == 0 {
                return;
            }
            state.token_input = input_tokens;
            state.token_output = output_tokens;
            state.token_cached = cached_input_tokens;
            // `input_tokens` is the full prompt sent this round; adding the
            // output gives the window occupied after the reply. The latest
            // round is the largest, so replace rather than accumulate.
            state.context_tokens = input_tokens.saturating_add(output_tokens);
        }
        RuntimeEvent::TurnProgress {
            phase,
            closing,
            no_progress_streak,
            closeout_deny_rounds: _,
        } => {
            mark_turn_busy(state);
            // Coarse chrome only — no tool dumps. Closing / thrash streaks
            // surface in the activity line so remote/local share one signal.
            if closing {
                state.activity = Some(format!("收口中 · {phase}"));
            } else if no_progress_streak > 0 {
                state.activity = Some(format!("无进展 ×{no_progress_streak} · {phase}"));
            }
        }
        RuntimeEvent::TurnCompleted => {
            finish_turn(state, TurnEndStatus::Completed, None);
        }
        RuntimeEvent::TurnAnswered => {
            finish_turn(state, TurnEndStatus::Answered, None);
        }
        RuntimeEvent::TurnTruncated { error } => {
            finish_turn(state, TurnEndStatus::Truncated, Some(error));
            state.notification = None;
        }
        RuntimeEvent::TurnIncomplete { reason } => {
            // The durable turn-end marker is the single source of truth. A
            // second transient notification repeats the same reason onscreen.
            finish_turn(state, TurnEndStatus::Incomplete, Some(reason));
            state.notification = None;
        }
        RuntimeEvent::TurnCompletedUnverified { reason } => {
            finish_turn(state, TurnEndStatus::Unverified, Some(reason));
            state.notification = None;
        }
        RuntimeEvent::TurnFailed { error } => {
            state.status = RuntimeStatus::Error;
            state.activity = None;
            state.goal_mode_active = false;
            state.transcript.finalize_in_flight();
            state.cancel_armed = false;
            state.force_cancel_armed = false;
            seal_analysis_segment(state);
            // Surface the error ONCE, followed by the persistent turn-end
            // marker. A duplicate transient notification would only add noise.
            let detail = error.clone();
            state.transcript.push_error(error);
            state.transcript.push_turn_end(
                TurnEndStatus::Failed,
                state.turn_tool_calls,
                state.elapsed_secs,
                turn_end_summary(state, TurnEndStatus::Failed),
                Some(detail),
            );
        }
        RuntimeEvent::TurnCancelled => {
            state.status = RuntimeStatus::Idle;
            state.activity = None;
            state.goal_mode_active = false;
            state.transcript.finalize_in_flight();
            state.cancel_armed = false;
            state.force_cancel_armed = false;
            let summary = turn_end_summary(state, TurnEndStatus::Cancelled);
            state.transcript.push_turn_end(
                TurnEndStatus::Cancelled,
                state.turn_tool_calls,
                state.elapsed_secs,
                summary,
                None,
            );
            seal_analysis_segment(state);
            state.notification = Some(Notification {
                level: NotificationLevel::Warning,
                message: state.t().cancelled_continue.to_string(),
            });
        }
        RuntimeEvent::SubAgentUpdated {
            id,
            nickname,
            role,
            done,
            ok,
            detail,
            profile_id,
            profile_role: _,
            capabilities,
            contribution,
        } => {
            let started = state.elapsed_secs;
            state.team.apply_update(crate::multi_agent::ChildUpdate {
                id: id.clone(),
                nickname: nickname.clone(),
                role: role.clone(),
                done,
                ok,
                detail: detail.clone(),
                profile_id,
                capabilities,
                contribution: contribution.clone(),
                started_elapsed_secs: started,
            });
            if done {
                // The projection is the source of truth now, not a count
                // scraped out of the parent-facing summary.
                let projected = state
                    .team
                    .children
                    .iter()
                    .find(|c| c.id == id)
                    .map(|c| c.contribution.clone())
                    .unwrap_or(crate::multi_agent::Contribution::NotMeasured);
                state
                    .transcript
                    .complete_sub_agent_with_contribution(&id, &nickname, ok, detail, projected);
            } else {
                state
                    .transcript
                    .push_sub_agent_started(id, nickname, role, detail, started);
            }
        }
        RuntimeEvent::UnfinishedGoalsLoaded { goals, .. } => {
            state.unfinished_goals = goals;
        }
        RuntimeEvent::ChildContributionLoaded { detail, .. } => {
            state.team.apply_detail(detail);
        }
        RuntimeEvent::SubAgentProgress {
            id,
            active,
            input_tokens,
            output_tokens,
            cached_input_tokens,
        } => state.transcript.update_sub_agent_progress(
            &id,
            active,
            input_tokens,
            output_tokens,
            cached_input_tokens,
        ),
        RuntimeEvent::SubAgentActivity {
            id,
            phase,
            tool,
            is_error,
            ..
        } => {
            let step = if phase == "tool_finished" {
                if is_error {
                    format!("{tool} ✗")
                } else {
                    format!("{tool} ✓")
                }
            } else {
                tool
            };
            state.transcript.update_sub_agent_activity(&id, step);
        }
        RuntimeEvent::MemoryList {
            memory_dir,
            active,
            archived,
            pending,
        } => {
            // Multi-line list must live in the transcript (status line is 1 row +
            // Info TTL ~4s). Users need to see every entry and forget ids.
            let mut lines = vec![
                "Memory".to_string(),
                format!("memory_dir={memory_dir}"),
                format!("active ({})", active.len()),
            ];
            if active.is_empty() {
                lines.push("  (none)".into());
            } else {
                for e in &active {
                    lines.push(format!("  [{}] {}", e.id, e.title));
                }
            }
            lines.push(format!("archived ({})", archived.len()));
            if archived.is_empty() {
                lines.push("  (none)".into());
            } else {
                for e in &archived {
                    lines.push(format!("  [{}] {}", e.id, e.title));
                }
            }
            // Pending candidates were previously invisible here, so the only
            // way to adopt one was the CLI. Listing them is what makes the
            // consent gate usable from the UI it gates.
            lines.push(format!("pending ({})", pending.len()));
            if pending.is_empty() {
                lines.push("  (none)".into());
            } else {
                for e in &pending {
                    lines.push(format!("  [{}] {}", e.id, e.title));
                }
            }
            lines.push(if pending.is_empty() {
                "hint: /memory forget <id>".into()
            } else {
                "hint: /memory accept <id> · /memory forget <id>".to_string()
            });
            state.transcript.push_note(lines.join("\n"));
            state.notification = Some(Notification {
                level: NotificationLevel::Info,
                message: format!(
                    "memory · active={} pending={} archived={}",
                    active.len(),
                    pending.len(),
                    archived.len()
                ),
            });
        }
        RuntimeEvent::UserShellStarted {
            execution_id,
            command,
            cwd,
        } => {
            state.transcript.push_user_shell_started(
                execution_id.clone(),
                command,
                cwd,
                state.elapsed_secs as i64,
            );
            // Focus the Details screen on this execution (the `!` submit
            // already switched to it).
            state.shell_screen_item = state.transcript.user_shell_index(&execution_id);
        }
        RuntimeEvent::UserShellOutput {
            execution_id,
            chunk,
            ..
        } => {
            // Sanitize BEFORE it enters any buffer: shell output is
            // untrusted terminal text (ANSI/OSC/control sequences).
            let clean = leveler_core::sanitize_terminal_output(&chunk);
            state
                .transcript
                .append_user_shell_output(&execution_id, &clean);
        }
        RuntimeEvent::UserShellExited {
            execution_id,
            exit_code,
            duration_ms,
            status,
        } => {
            state.transcript.complete_user_shell(
                &execution_id,
                exit_code,
                duration_ms,
                crate::transcript::UserShellStatus::from_wire(&status),
            );
        }
        RuntimeEvent::ContextCompacted { from, to } => {
            // Client-owned wording for the structured runtime fact.
            state.notification = Some(Notification {
                level: NotificationLevel::Info,
                message: state
                    .t()
                    .context_compacted
                    .replace("{from}", &from.to_string())
                    .replace("{to}", &to.to_string()),
            });
        }
        RuntimeEvent::ContextExpanded {
            from_tokens,
            to_tokens,
            ..
        } => {
            state.notification = Some(Notification {
                level: NotificationLevel::Info,
                message: state
                    .t()
                    .context_expanded
                    .replace("{from}", &from_tokens.to_string())
                    .replace("{to}", &to_tokens.to_string()),
            });
        }
        RuntimeEvent::Notification { level, message } => {
            // Errors stick until Esc / next turn; also land in the transcript so
            // a glance away cannot lose them to the status TTL.
            if level == NotificationLevel::Error {
                state.transcript.push_error(message.clone());
            }
            state.notification = Some(Notification { level, message });
        }
        RuntimeEvent::BtwStarted { question } => {
            state.transcript.begin_btw(question);
            state.activity = Some(state.t().btw_label.to_string());
        }
        RuntimeEvent::BtwTextDelta { delta } => {
            state.transcript.append_btw(&delta);
        }
        RuntimeEvent::BtwCompleted => {
            state.transcript.finish_btw(false);
            if state.activity.as_deref() == Some(state.t().btw_label) {
                state.activity = None;
            }
        }
        RuntimeEvent::BtwFailed { error } => {
            state
                .transcript
                .append_btw(&format!("{}: {error}", state.t().btw_failed));
            state.transcript.finish_btw(true);
            if state.activity.as_deref() == Some(state.t().btw_label) {
                state.activity = None;
            }
            state.notification = Some(Notification {
                level: NotificationLevel::Error,
                message: error,
            });
        }
        RuntimeEvent::BackgroundTaskStarted {
            task_id,
            program,
            args,
        } => {
            // One lifecycle, one human label. The id stays as the projection
            // key (stable identity, never text matching); what the user reads
            // is the command it is running.
            let label = crate::render::tool_summary(
                "run_command",
                &serde_json::json!({ "program": program, "args": args }).to_string(),
            );
            let label = if label.is_empty() {
                program.clone()
            } else {
                label
            };
            state
                .background_task_labels
                .insert(task_id.clone(), label.clone());
            state.notification = Some(Notification {
                level: NotificationLevel::Info,
                message: state.t().background_task_started.replace("{}", &label),
            });
        }
        RuntimeEvent::ObservabilityLoaded {
            observation,
            query_id,
        } => {
            // Only a correlated 1.6 response for the query this view owns.
            // `query_id: None` is a legacy 1.5 payload — decode-safe, not owned.
            match (&state.trace.pending_query_id, &query_id) {
                (Some(pending), Some(incoming)) if pending == incoming => {}
                _ => return,
            }
            state.trace.loaded = Some(observation);
            state.trace.clamp();
        }
        RuntimeEvent::BackgroundTaskExited {
            task_id,
            exit_code,
            ok,
            ..
        } => {
            let t = state.t();
            // The same lifecycle settling: name what finished, not its id.
            // A missing label (exit seen without its start, e.g. after a
            // reconnect) falls back to a truthful generic — never invented.
            let label = state
                .background_task_labels
                .remove(&task_id)
                .unwrap_or_else(|| t.background_task_generic.to_string());
            let message = if ok {
                t.background_task_done.replace("{}", &label)
            } else {
                let failed = t.background_task_failed.replace("{}", &label);
                match exit_code {
                    Some(code) => format!("{failed} · exit {code}"),
                    None => failed,
                }
            };
            state.notification = Some(Notification {
                level: if ok {
                    NotificationLevel::Info
                } else {
                    NotificationLevel::Warning
                },
                message,
            });
        }
    }
}

/// Drop any parked interaction the predicate matches — used when a resolution
/// event names a request that is queued behind the active overlay rather than
/// showing on screen.
fn dismiss_resolved_interaction(
    state: &mut AppState,
    matches_resolved: impl Fn(&PendingInteraction) -> bool,
) {
    state
        .pending_interactions
        .retain(|pending| !matches_resolved(pending));
}

fn finish_turn(state: &mut AppState, status: TurnEndStatus, detail: Option<String>) {
    state.status = RuntimeStatus::Idle;
    state.activity = None;
    state.goal_mode_active = false;
    state.transcript.finalize_in_flight();
    state.cancel_armed = false;
    state.force_cancel_armed = false;
    // Answer is in the transcript; a fully-done plan chrome is pure clutter.
    if state
        .plan
        .as_ref()
        .is_some_and(|p| !crate::workbench::plan_panel_should_show(p))
    {
        state.plan = None;
    }
    // If the provider never reported usage, still drive the context gauge from
    // the visible transcript so it is not stuck at empty capacity forever.
    if state.context_tokens == 0 && state.token_input == 0 {
        let estimated = estimate_transcript_tokens(state);
        if estimated > 0 {
            state.context_tokens = estimated;
        }
    }
    let handoff = state.transcript.latest_turn_handoff();
    let suggestion = handoff
        .as_ref()
        .map(|handoff| handoff.next_step.clone())
        .or_else(|| (status == TurnEndStatus::Incomplete).then(|| "继续".to_string()));
    let summary = turn_end_summary(state, status);
    state.transcript.push_turn_end(
        status,
        state.turn_tool_calls,
        state.elapsed_secs,
        summary,
        detail,
    );
    if status != TurnEndStatus::Cancelled
        && let Some(handoff) = handoff
    {
        state.transcript.push_recap(handoff);
    }
    if status != TurnEndStatus::Cancelled
        && state.composer.is_empty()
        && let Some(suggestion) = suggestion
    {
        state.composer.replace_suggestion(suggestion);
    }
    seal_analysis_segment(state);
}

/// Rough token estimate from transcript text (CJK-aware), used only when the
/// model never reported real usage for the turn.
fn estimate_transcript_tokens(state: &AppState) -> u32 {
    use crate::transcript::TranscriptItem;
    let mut total = 0u32;
    for item in state.transcript.items() {
        match item {
            TranscriptItem::User(text) | TranscriptItem::Error(text) => {
                total = total.saturating_add(estimate_text_tokens(text));
            }
            TranscriptItem::Assistant(b) => {
                total = total.saturating_add(estimate_text_tokens(&b.text));
            }
            TranscriptItem::ToolGroup(g) => {
                for call in &g.calls {
                    total = total.saturating_add(estimate_text_tokens(&call.arguments));
                    if let Some(p) = &call.preview {
                        total = total.saturating_add(estimate_text_tokens(p));
                    }
                }
            }
            // Side questions are ephemeral and not part of main context usage.
            TranscriptItem::Btw(_) => {}
            TranscriptItem::Recap(_) => {}
            _ => {}
        }
    }
    total
}

fn estimate_text_tokens(text: &str) -> u32 {
    let (mut cjk, mut other) = (0u32, 0u32);
    for ch in text.chars() {
        if ch as u32 >= 0x2E80 {
            cjk += 1;
        } else {
            other += 1;
        }
    }
    (cjk as f32 / 1.6 + other as f32 / 4.0).ceil() as u32
}

/// Compact product summary for the turn-end marker (files / verify).
///
/// Success verify chrome (`verify ✓`) is **outcome-gated**: an Unverified turn
/// must never show it even when gate `passed` is true (`passed` means
/// !Failed, not Verdict::Verified).
fn turn_end_summary(state: &AppState, status: TurnEndStatus) -> Option<String> {
    let mut parts = Vec::new();
    if let Some(diff) = &state.diff
        && !diff.files.is_empty()
    {
        parts.push(format!("{} files", diff.files.len()));
    }
    // Unverified / incomplete / failed / cancelled: no success verify mark.
    let allow_success_verify = matches!(status, TurnEndStatus::Completed | TurnEndStatus::Answered);
    // Open plan steps travel with the summary: a green gate line must not read
    // as full task success while the plan panel says work remains (R004 F4).
    let plan_open = state.plan.as_ref().and_then(|p| {
        let (k, n) = crate::workbench::plan_done_total(p);
        (n > 0 && k < n).then_some((k, n))
    });
    if let Some(v) = &state.verification {
        if let Some(passed) = v.passed {
            if passed {
                if allow_success_verify {
                    // Prefer strict green: all listed checks Passed (and non-empty).
                    let all_passed = !v.checks.is_empty()
                        && v.checks
                            .iter()
                            .all(|c| c.status == leveler_client_protocol::CheckState::Passed);
                    if all_passed || v.checks.is_empty() {
                        parts.push("verify ✓".to_string());
                    } else {
                        let ok = v
                            .checks
                            .iter()
                            .filter(|c| c.status == leveler_client_protocol::CheckState::Passed)
                            .count();
                        parts.push(format!("verify {ok}/{}", v.checks.len()));
                    }
                }
                // else: Unverified turn — omit success chrome entirely
            } else {
                parts.push("verify ✗".to_string());
            }
        } else if !v.checks.is_empty() {
            let ok = v
                .checks
                .iter()
                .filter(|c| c.status == leveler_client_protocol::CheckState::Passed)
                .count();
            parts.push(format!("verify {ok}/{}", v.checks.len()));
        }
    }
    if let Some((k, n)) = plan_open {
        parts.push(format!("计划 {k}/{n}"));
    }
    if parts.is_empty() {
        None
    } else {
        Some(parts.join(" · "))
    }
}

pub(super) fn start_turn(state: &mut AppState) {
    state.turn_tool_calls = 0;
    state.status = RuntimeStatus::Busy;
    state.project_rule_sources.clear();
    seal_analysis_segment(state);
}

/// A segment boundary (tool start, assistant start, turn end): the live
/// reasoning scratch is spent — drop it. Nothing renders it, nothing keeps
/// it; only the status line's thinking indicator ever read it.
fn seal_analysis_segment(state: &mut AppState) {
    state.live_reasoning.clear();
}

fn mark_turn_busy(state: &mut AppState) {
    if !state.is_busy() {
        start_turn(state);
    }
}

/// Update header metadata from a snapshot without touching the transcript.
fn apply_meta(state: &mut AppState, session: &UiSessionSnapshot) {
    if state.repository != session.repository {
        state.file_candidates.clear();
        state.file_index_requested = false;
        // Skill slash catalog is rooted on the repo; drop cache so the next
        // `/` rescans project + user skills for the new workspace.
        state.skill_catalog.clear();
        state.skill_catalog_root = None;
    }
    state.session_id = session.id.clone();
    state.repository = session.repository.clone();
    state.branch = session.branch.clone();
    state.model_label = session
        .model
        .as_ref()
        .map(|m| m.to_string())
        .unwrap_or_else(|| "Auto".to_string());
    state.mode = session.mode;
    state.mode_label = mode_label(session.mode).to_string();
    state.available_models = session.available_models.clone();
    state.vision = session.vision;
    if let Some(reasoning) = &session.reasoning {
        state.reasoning_effort = reasoning.effective.clone();
    }
    // Product axes: the session record is the source of truth; adopt when the
    // runtime sends them so a reconnect cannot show a stale local guess. Old
    // runtimes omit the fields — keep the local value then.
    if let Some(work_profile) = &session.work_profile {
        state.work_profile = work_profile.clone();
    }
    if let Some(collaboration) = &session.collaboration {
        state.collaboration = collaboration.clone();
    }
}

fn apply_session(state: &mut AppState, session: UiSessionSnapshot) {
    // Only a switch to a DIFFERENT session resets per-session view state; a
    // same-session resync (e.g. after a broadcast lag) must keep live plan/diff/
    // token state intact.
    let switching = state.session_id != session.id;
    apply_meta(state, &session);
    state.status = match session.status.as_str() {
        "running" => RuntimeStatus::Busy,
        "failed" => RuntimeStatus::Error,
        _ => RuntimeStatus::Idle,
    };

    // A reconnect snapshot replaces the live control queue. Only in-process
    // waiters are included, so stale requests from interrupted turns are never
    // resurrected after a runtime restart.
    if matches!(
        state.overlay,
        Some(Overlay::Approval(_)) | Some(Overlay::Clarification(_))
    ) {
        state.overlay = None;
    }
    state.pending_interactions.clear();
    for interaction in session.pending_interactions.iter().cloned() {
        let pending = match interaction {
            leveler_client_protocol::UiPendingInteraction::Approval(request) => {
                PendingInteraction::Approval(request)
            }
            leveler_client_protocol::UiPendingInteraction::Clarification(request) => {
                PendingInteraction::Clarification(request)
            }
        };
        if state.overlay.is_none() {
            state.overlay = Some(match pending {
                PendingInteraction::Approval(request) => {
                    Overlay::Approval(Box::new(ApprovalOverlay::new(request)))
                }
                PendingInteraction::Clarification(request) => {
                    Overlay::Clarification(Box::new(ClarificationOverlay::new(request)))
                }
            });
        } else {
            state.pending_interactions.push_back(pending);
        }
    }

    state.plan = session
        .plan
        .filter(crate::workbench::plan_panel_should_show);
    state.verification = session.verification.clone();
    state.diff = session.diff.clone();
    if state
        .diff
        .as_ref()
        .is_none_or(|diff| state.diff_selected >= diff.files.len())
    {
        state.diff_selected = 0;
    }
    state.checkpoints = session.checkpoints.clone();

    if switching {
        state.context_files.clear();
        state.context_tokens = 0;
        state.token_input = 0;
        state.token_output = 0;
        state.token_cached = 0;
        state.project_rule_sources.clear();
        state.background_task_labels.clear();
        seal_analysis_segment(state);
        state.activity = None;
        state.turn_tool_calls = 0;
        state.screen_scroll = 0;
        state.pending_attachments.clear();
    }

    // Rebuild the transcript from the session's persisted messages. Opening a
    // different session (or a lagged resync) replaces the current view.
    state.transcript.clear();
    for message in &session.messages {
        match message.role {
            // A compacted-history summary is stored as a User message so the model
            // keeps it as context, but it isn't something the user typed — render
            // it as a distinct assistant/summary block, not a user turn.
            UiRole::User
                if message
                    .text
                    .starts_with(leveler_client_protocol::COMPACTION_SUMMARY_PREFIX) =>
            {
                state.transcript.begin_assistant(message.id.clone());
                state
                    .transcript
                    .append_assistant(&message.id, &message.text);
                state.transcript.finish_assistant(&message.id);
            }
            UiRole::User => state.transcript.push_user(message.text.clone()),
            UiRole::Assistant => {
                state.transcript.begin_assistant(message.id.clone());
                state
                    .transcript
                    .append_assistant(&message.id, &message.text);
                state.transcript.finish_assistant(&message.id);
            }
            UiRole::System | UiRole::Tool => {}
        }
    }
    // User shell executions (history + a still-running one) survive
    // reconnect via the snapshot; blocks are rebuilt in order.
    state.shell_screen_item = None;
    for shell in &session.user_shells {
        let running = shell.status == "running";
        // Back-date the start (possibly below zero) so the live runtime
        // keeps counting from the runtime's elapsed, not from zero at
        // reconnect.
        let started = state.elapsed_secs as i64 - shell.elapsed_secs as i64;
        state.transcript.push_user_shell_started(
            shell.id.clone(),
            shell.command.clone(),
            shell.cwd.clone(),
            started,
        );
        if !shell.output_tail.is_empty() {
            let clean = leveler_core::sanitize_terminal_output(&shell.output_tail);
            state.transcript.append_user_shell_output(&shell.id, &clean);
        }
        if !running {
            state.transcript.complete_user_shell(
                &shell.id,
                shell.exit_code,
                shell.elapsed_secs * 1000,
                crate::transcript::UserShellStatus::from_wire(&shell.status),
            );
        } else {
            state.shell_screen_item = state.transcript.user_shell_index(&shell.id);
        }
    }
    if let Some(report) = session.completion_report {
        state.transcript.push_completion(report);
    }
    state.turn_tool_calls = session.active_tools.len();
    for tool in session.active_tools {
        // Restored mid-flight calls are shown as normal (not grouped as parallel);
        // the snapshot does not carry the batch flag.
        let started = state.elapsed_secs;
        state
            .transcript
            .push_tool_started(tool.id, tool.name, tool.arguments, false, started);
    }

    // Welcome card removed: Header owns project context; Input owns model/mode.
}

pub(super) fn mode_label(mode: PermissionProfile) -> &'static str {
    match mode {
        PermissionProfile::RequestApproval => "RequestApproval",
        PermissionProfile::Assisted => "Assisted",
        PermissionProfile::FullAccess => "FullAccess",
    }
}
