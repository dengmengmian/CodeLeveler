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
        RuntimeEvent::SessionHistoryLoaded {
            query_id,
            session_id,
            entries,
            omitted_turns,
        } => {
            let ours = query_id.is_some() && query_id == state.history_query;
            if ours {
                state.history_query = None;
            }
            // Only the answer to this client's own query, for the session on
            // screen, and never over a turn that is running now.
            // A user shell still running is live state the log has not closed.
            if ours
                && session_id == state.session_id
                && !state.is_busy()
                && state.shell_screen_item.is_none()
                && !entries.is_empty()
            {
                replay_history(state, entries, omitted_turns);
            }
        }
        RuntimeEvent::SessionUpdated { session } => {
            let previous = state.mode;
            apply_meta(state, &session);
            if state.pending_permission == Some(state.mode) {
                state.pending_permission = None;
            }
            if previous != state.mode {
                let t = state.t();
                let human = match state.mode {
                    PermissionProfile::RequestApproval => t.perm_readonly,
                    PermissionProfile::Assisted => t.perm_workspace,
                    PermissionProfile::FullAccess => t.perm_full,
                };
                state.notification = Some(crate::state::Notification {
                    level: if state.mode == PermissionProfile::FullAccess {
                        NotificationLevel::Warning
                    } else {
                        NotificationLevel::Info
                    },
                    message: format!("{}: {human}", t.overlay_mode),
                });
            }
        }
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
            // Staging an attachment is the start of a new message, however the
            // user got here (clipboard image, `/image`, `/attach`). The last
            // turn's next step no longer describes what they are composing.
            crate::suggestion::clear(state);
            // The image goes where the user is writing, as `[图片 #N]`, and
            // takes the place among the staged images that its token has in
            // the sentence — paste one ahead of another and it IS the first.
            let index = state.composer.insert_image_token();
            let index = index.min(state.pending_attachments.len());
            state.pending_attachments.insert(index, attachment);
        }
        RuntimeEvent::AttachmentProcessingFailed { error } => {
            state.notification = Some(Notification {
                level: NotificationLevel::Error,
                message: format!("附件处理失败: {error}"),
            });
        }
        RuntimeEvent::UserMessageAdded { message } => {
            let shown = state.message_with_images(message.images, &message.text);
            state.transcript.push_user_if_new(shown);
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
            // A fresh attempt began. If it follows a retry, the transport is
            // reachable again: confirm that briefly, then the normal streaming
            // status takes over. The retry decision stays the runtime's.
            if state.reconnecting.take().is_some() {
                state.reconnected_until =
                    Some(std::time::Instant::now() + crate::state::RECONNECTED_NOTICE);
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
        RuntimeEvent::TurnFinalizing { stage } => {
            mark_turn_busy(state);
            state.finalization_stage = Some(stage);
            // A command heartbeat belongs to the command that just ended. The
            // typed finalization stage now owns the status line and its clock.
            clear_activity(state);
        }
        RuntimeEvent::AgentActivity { label } => {
            mark_turn_busy(state);
            state.activity = Some(label);
            state.activity_elapsed_secs = None;
        }
        RuntimeEvent::CommandProgress { label, elapsed_ms } => {
            // Long-command heartbeat: name the running command with a live
            // elapsed so the status line reads "运行 cargo test · 02:31" instead
            // of a bare "等待模型". Reuses the activity slot (single source).
            mark_turn_busy(state);
            // The command owns this elapsed. The status line shows it INSTEAD
            // of the turn's, so a running command reads as one duration rather
            // than two adjacent unlabelled ones.
            state.activity = Some(state.t().running_command.replace("{}", &label));
            state.activity_elapsed_secs = Some(elapsed_ms / 1000);
        }
        RuntimeEvent::ModelRetrying {
            attempt,
            max_attempts,
            delay_ms,
        } => {
            // Connectivity belongs in ephemeral status, NOT the transcript: a
            // brief network blip must not spam the conversation. The retry
            // controller owns the decision; this only reflects it, including
            // the exact delay it announced, so the countdown cannot disagree
            // with what the scheduler actually waits. Whatever streamed before
            // the blip is discarded by the retry's `AssistantAttemptReset`, so
            // no stale text lingers.
            mark_turn_busy(state);
            let delay = std::time::Duration::from_millis(delay_ms);
            state.reconnecting = Some(crate::state::Reconnecting {
                attempt,
                max_attempts,
                delay,
                retry_at: std::time::Instant::now() + delay,
            });
            state.reconnected_until = None;
            state.activity_elapsed_secs = None;
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
            let target = crate::render::tool_summary_for(&name, &arguments, state.t());
            // A command owns its own clock from the moment it starts. Leaving
            // this `None` made the status line append the TURN's elapsed to the
            // command's name, so a `node --version` that ran 0.12s at turn
            // elapsed 294s read as "执行命令 node --version · 4m 54s" — while
            // its own row, in the same frame, read "运行中 · 0s". The
            // heartbeat then keeps this number authoritative.
            state.activity_elapsed_secs = (crate::tool_taxonomy::activity_class(&name)
                == crate::tool_taxonomy::ActivityClass::Shell)
                .then_some(0);
            state.activity = Some(if target.is_empty() || target == "{}" {
                verb
            } else {
                format!("{verb} {target}")
            });
            let started = state.elapsed_secs;
            state
                .transcript
                .push_tool_started(id, name, arguments, parallel, started as i64);
        }
        RuntimeEvent::ToolCallCompleted {
            id,
            ok,
            preview,
            duration_ms,
            applied_diff,
            exit_code,
            stop,
        } => {
            // Strip ANSI and controls so vitest/npm color codes do not show as
            // `[32m` garbage when ESC was already dropped (cell TUI).
            let preview = leveler_core::sanitize_terminal_output(&preview);
            state.transcript.complete_command(
                &id,
                ok,
                preview,
                duration_ms,
                applied_diff,
                exit_code,
                stop,
            );
            // The tool is done; leaving its label up while the model thinks
            // reads as a hung tool ("读取 x… (4m)"). Fall back to the
            // thinking indicator until the next activity arrives.
            clear_activity(state);
            seal_analysis_segment(state);
        }
        RuntimeEvent::ToolCallOutput { id, chunk, .. } => {
            // Already sanitized by the runtime; strip again like every other
            // terminal text this client paints.
            let chunk = leveler_core::sanitize_terminal_output(&chunk);
            state.transcript.append_tool_output(&id, &chunk);
        }
        RuntimeEvent::PlanUpdated { plan } => {
            // This is the runtime's latest declaration for the active turn.
            // Rendering may hide a fully-done plan, but the reducer keeps the
            // exact value until the terminal transition archives it.
            state.plan = Some(plan);
        }
        RuntimeEvent::VerificationUpdated { verification } => {
            state.turn_verification = Some(verification.clone());
            state.verification = Some(verification);
        }
        RuntimeEvent::DiffUpdated { diff } => {
            state.turn_diff_files = Some(diff.files.len());
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
        RuntimeEvent::TurnCompletedWithWarnings { reason } => {
            finish_turn(state, TurnEndStatus::CompletedWithWarnings, Some(reason));
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
        RuntimeEvent::TurnCompletedChecksFailed { reason } => {
            finish_turn(state, TurnEndStatus::ChecksFailed, Some(reason));
            state.notification = None;
        }
        RuntimeEvent::TurnFailed { error, failure } => {
            state.status = RuntimeStatus::Error;
            state.finalization_stage = None;
            clear_activity(state);
            state.goal_mode_active = false;
            state.transcript.finalize_in_flight();
            state.team.mark_unreported_at_turn_end(state.elapsed_secs);
            state.cancel_armed = false;
            state.force_cancel_armed = false;
            seal_analysis_segment(state);
            // ONE terminal failure → ONE primary block. The turn-end marker
            // states the outcome but must not carry a second copy of the
            // failure text.
            let block = {
                let t = state.t();
                match &failure {
                    Some(f) => crate::transcript::FailureBlock {
                        title: t.failure_title.to_string(),
                        summary: failure_summary(f, t),
                        subtitle: failure_subtitle(f),
                        detail: failure_detail_text(f, t),
                        expanded: false,
                    },
                    // A failure the runtime could not type (legacy event, or a
                    // non-provider failure): show the string as the summary.
                    None => crate::transcript::FailureBlock {
                        title: t.failure_title.to_string(),
                        summary: error.clone(),
                        subtitle: None,
                        detail: String::new(),
                        expanded: false,
                    },
                }
            };
            state.transcript.push_failure(block);
            let summary = turn_end_summary(state, TurnEndStatus::Failed);
            archive_active_plan(state);
            state.transcript.push_turn_end(
                TurnEndStatus::Failed,
                state.turn_tool_calls,
                state.elapsed_secs,
                summary,
                None,
            );
            state.turn_verification = None;
            state.turn_diff_files = None;
        }
        RuntimeEvent::TurnCancelled => {
            state.status = RuntimeStatus::Idle;
            state.finalization_stage = None;
            clear_activity(state);
            state.goal_mode_active = false;
            state.transcript.finalize_in_flight();
            state.team.mark_unreported_at_turn_end(state.elapsed_secs);
            state.cancel_armed = false;
            state.force_cancel_armed = false;
            let summary = turn_end_summary(state, TurnEndStatus::Cancelled);
            archive_active_plan(state);
            state.turn_verification = None;
            state.turn_diff_files = None;
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
            title,
            done,
            ok,
            detail,
            profile_id,
            profile_role: _,
            read_only,
            agent,
            contribution,
            stop,
            limit,
            ..
        } => {
            let started = state.elapsed_secs;
            let agent_name = agent.as_ref().map(|a| a.name.clone());
            state.team.apply_update(crate::multi_agent::ChildUpdate {
                id: id.clone(),
                nickname: nickname.clone(),
                role: role.clone(),
                done,
                ok,
                detail: detail.clone(),
                title,
                profile_id,
                agent_name: agent_name.clone(),
                read_only,
                contribution: contribution.clone(),
                started_elapsed_secs: started,
                stop,
                limit,
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
                state.transcript.complete_sub_agent_with_contribution(
                    &id, &nickname, ok, detail, projected, stop, limit,
                );
            } else {
                state.transcript.push_sub_agent_started(
                    id.clone(),
                    nickname,
                    role,
                    detail,
                    started,
                );
            }
            state.transcript.set_sub_agent_agent_name(&id, agent_name);
        }
        RuntimeEvent::UnfinishedGoalsLoaded { goals, .. } => {
            state.unfinished_goals = goals;
        }
        RuntimeEvent::AgentsLoaded {
            agents, problems, ..
        } => {
            let t = state.t();
            state
                .transcript
                .push_note(crate::agents_view::listing_note(&agents, &problems, t));
        }
        RuntimeEvent::AgentLoaded {
            name, agent, error, ..
        } => {
            let note = match (agent, error) {
                (Some(detail), _) => crate::agents_view::detail_note(&detail, state.t()),
                (None, Some(error)) => error,
                (None, None) => format!("Agent \"{name}\" not found."),
            };
            state.transcript.push_note(note);
        }
        // Another client changed the registry; the TUI does not write agents.
        // A failure is still said, so a user watching this session sees it.
        RuntimeEvent::AgentMutated {
            name, ok, error, ..
        } => {
            if !ok {
                state.notification = Some(Notification {
                    level: NotificationLevel::Error,
                    message: format!("agent {name}: {}", error.unwrap_or_default()),
                });
            }
        }
        RuntimeEvent::GoalRecapCreated { recap } => {
            // History, never the lower runtime stack. Idempotent on
            // checkpoint_id inside push_goal_recap.
            state.transcript.push_goal_recap(recap);
        }
        RuntimeEvent::ChildContributionLoaded { detail, .. } => {
            state.team.apply_detail(detail);
        }
        RuntimeEvent::SubAgentStateChanged {
            id,
            state: child_state,
        } => {
            state.team.apply_state(&id, child_state, state.elapsed_secs);
            state.transcript.set_sub_agent_state(&id, child_state);
        }
        RuntimeEvent::SubAgentProgress {
            id,
            active,
            input_tokens,
            output_tokens,
            cached_input_tokens,
        } => {
            // Feed BOTH projections: the transcript block and the runtime
            // roster (elapsed · usage column).
            state
                .team
                .apply_progress(&id, active, input_tokens, output_tokens);
            state.transcript.update_sub_agent_progress(
                &id,
                active,
                input_tokens,
                output_tokens,
                cached_input_tokens,
            )
        }
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
            state.team.apply_activity(&id, &step);
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
            let t = state.t();
            let mut lines = vec![
                t.memory_title.to_string(),
                format!("memory_dir={memory_dir}"),
                format!("{} ({})", t.memory_active, active.len()),
            ];
            if active.is_empty() {
                lines.push(format!("  {}", t.memory_none));
            } else {
                for e in &active {
                    // A sensitive entry is kept but withheld from the model;
                    // saying so is the difference between "stored" and "used".
                    let mut row = format!("  [{}] {}", e.id, e.title);
                    if let Some(kind) = &e.kind {
                        row.push_str(&format!(" ({})", memory_kind_label(*kind)));
                    }
                    if e.sensitive {
                        row.push_str(" · 敏感内容，不提供给模型");
                    }
                    lines.push(row);
                }
            }
            lines.push(format!("{} ({})", t.memory_archived, archived.len()));
            if archived.is_empty() {
                lines.push(format!("  {}", t.memory_none));
            } else {
                for e in &archived {
                    lines.push(format!("  [{}] {}", e.id, e.title));
                }
            }
            // Pending candidates were previously invisible here, so the only
            // way to adopt one was the CLI. Listing them is what makes the
            // consent gate usable from the UI it gates.
            lines.push(format!("{} ({})", t.memory_pending, pending.len()));
            if pending.is_empty() {
                lines.push(format!("  {}", t.memory_none));
            } else {
                for e in &pending {
                    lines.push(format!("  [{}] {}", e.id, e.title));
                    // Approving a title alone is not informed consent: show a
                    // compact preview of what would actually be stored.
                    let body = e.body.split_whitespace().collect::<Vec<_>>().join(" ");
                    let preview: String = body.chars().take(160).collect();
                    if !preview.is_empty() {
                        let ellipsis = if body.chars().count() > 160 {
                            "…"
                        } else {
                            ""
                        };
                        lines.push(format!("      {preview}{ellipsis}"));
                    }
                    lines.push(format!("      kind={} source={}", e.kind, e.source));
                }
            }
            lines.push(if pending.is_empty() {
                t.memory_hint_empty.to_string()
            } else {
                t.memory_hint_pending.to_string()
            });
            state.transcript.push_note(lines.join("\n"));
            // A listing summary must not overwrite a specific message that
            // just landed ("已保存记忆 [id]…"). The refresh follows the write,
            // and on a one-line status bar it would erase the only
            // confirmation the user gets.
            if state.notification.is_some() {
                return;
            }
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
            // Compaction is not a passing event: everything above this point is
            // a summary to the model now. A toast that fades leaves the reader
            // scrolling through detail the model no longer holds, so the
            // conversation keeps a line saying where that happened.
            state.transcript.push_note(
                state
                    .t()
                    .context_compacted
                    .replace("{from}", &from.to_string())
                    .replace("{to}", &to.to_string()),
            );
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
            state.btw.begin(question);
        }
        RuntimeEvent::BtwTextDelta { delta } => {
            state.btw.append(&delta);
        }
        RuntimeEvent::BtwCompleted => {
            state.btw.finish(crate::btw::BtwTurnState::Done, None);
        }
        RuntimeEvent::BtwCancelled => {
            state.btw.finish(crate::btw::BtwTurnState::Cancelled, None);
        }
        RuntimeEvent::BtwFailed { error } => {
            state
                .btw
                .finish(crate::btw::BtwTurnState::Failed, Some(&error));
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
            let label = crate::render::tool_summary_for(
                "run_command",
                &serde_json::json!({ "program": program, "args": args }).to_string(),
                state.t(),
            );
            let label = if label.is_empty() {
                program.clone()
            } else {
                label
            };
            state.background_task_labels.insert(
                task_id.clone(),
                crate::state::BackgroundTaskChrome::running(label.clone(), state.elapsed_secs),
            );
            state.notification = Some(Notification {
                level: NotificationLevel::Info,
                message: state.t().background_task_started.replace("{}", &label),
            });
        }
        RuntimeEvent::ContextUsage { accounting } => {
            // A live push from the runtime; always the freshest truth. Never a
            // TUI recomputation.
            state.context.loaded = Some(accounting);
            crate::context::clamp(&mut state.context);
        }
        RuntimeEvent::ContextLoaded {
            query_id,
            accounting,
        } => {
            // Only the answer to the query this view owns.
            let ours = query_id.is_some() && query_id == state.context.pending_query_id;
            if ours {
                state.context.pending_query_id = None;
                state.context.loaded = accounting;
                crate::context::clamp(&mut state.context);
            }
        }
        RuntimeEvent::ObservabilityLoaded {
            observation,
            query_id,
        } => {
            // Only a correlated current response for the query this view owns.
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
            duration_ms: _,
            ok,
        } => {
            let t = state.t();
            // Terminal is history, never active chrome. Retained registry
            // records remain available to `get`/`wait`, but the TUI's active
            // projection removes the task on the authoritative exit event.
            let label = state
                .background_task_labels
                .remove(&task_id)
                .map(|chrome| chrome.label)
                .unwrap_or_else(|| t.background_task_generic.to_string());
            if matches!(&state.activity_selected, Some(crate::activity::ActivityId::Background(open)) if open == &task_id)
            {
                state.activity_selected = None;
            }
            if matches!(&state.activity_open, Some(crate::activity::ActivityId::Background(open)) if open == &task_id)
            {
                state.activity_open = None;
            }
            let message = if ok {
                t.background_task_done.replace("{}", &label)
            } else {
                let failed = t.background_task_failed.replace("{}", &label);
                match exit_code {
                    Some(code) => format!("{failed} · exit {code}"),
                    None => failed,
                }
            };
            state.transcript.push_note(message.clone());
            state.notification = Some(Notification {
                level: if ok {
                    NotificationLevel::Info
                } else {
                    NotificationLevel::Warning
                },
                message,
            });
        }
        RuntimeEvent::BackgroundTasksReconciled { tasks } => {
            replace_active_background_tasks(state, &tasks);
        }
    }
}

/// Drop any parked interaction the predicate matches — used when a resolution
/// event names a request that is queued behind the active overlay rather than
/// Short label for a memory kind, so a listing says how an entry reaches the
/// model instead of making the reader remember the rules.
fn memory_kind_label(kind: leveler_client_protocol::UiMemoryKind) -> &'static str {
    use leveler_client_protocol::UiMemoryKind;
    match kind {
        UiMemoryKind::Preference => "长期偏好",
        UiMemoryKind::Decision => "决策",
        UiMemoryKind::Note => "笔记",
    }
}

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
    state.finalization_stage = None;
    clear_activity(state);
    state.goal_mode_active = false;
    state.transcript.finalize_in_flight();
    state.team.mark_unreported_at_turn_end(state.elapsed_secs);
    // §12: a clean outcome still needs an answer behind it. Only the two
    // outcomes that render as done are rewritten — an Incomplete or Failed turn
    // already says something went wrong.
    let answered = state.transcript.settle_final_answer();
    let status = match status {
        TurnEndStatus::Completed | TurnEndStatus::Answered if !answered => {
            TurnEndStatus::NoFinalAnswer
        }
        other => other,
    };
    state.cancel_armed = false;
    state.force_cancel_armed = false;
    // If the provider never reported usage, still drive the context gauge from
    // the visible transcript so it is not stuck at empty capacity forever.
    if state.context_tokens == 0 && state.token_input == 0 {
        let estimated = estimate_transcript_tokens(state);
        if estimated > 0 {
            state.context_tokens = estimated;
        }
    }
    // Read BEFORE `push_turn_end`: a TurnEnd marker is the scan boundary, so
    // the handoff has to be taken while this turn is still the latest one.
    let handoff = state.transcript.latest_turn_handoff();
    let suggestion = handoff
        .as_ref()
        .map(|handoff| handoff.next_step.clone())
        .or_else(|| {
            (status == TurnEndStatus::Incomplete).then(|| state.t().suggestion_continue.to_string())
        });
    let summary = turn_end_summary(state, status);
    archive_active_plan(state);
    state.turn_verification = None;
    state.turn_diff_files = None;
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
    // Ghost text, not input: the composer buffer stays exactly as the user
    // left it, and Tab is what moves the suggestion into it.
    match suggestion.filter(|_| status != TurnEndStatus::Cancelled) {
        Some(text) => crate::suggestion::offer(state, &text),
        None => crate::suggestion::clear(state),
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
/// Terminal states where the WORK itself is finished. Task outcome, plan
/// progress and verification outcome are three separate facts: a finished task
/// with a plan last reported at 6/9 and verification at 2/3 is entirely legal,
/// and the outcome is the runtime's to report, not the plan's to dispute.
fn work_is_finished(status: TurnEndStatus) -> bool {
    matches!(
        status,
        TurnEndStatus::Completed
            | TurnEndStatus::CompletedWithWarnings
            | TurnEndStatus::Answered
            | TurnEndStatus::Unverified
            | TurnEndStatus::ChecksFailed
    )
}

/// The always-visible failure line. When the runtime spent automatic retries
/// before giving up, the count rides here: a terminal network failure must
/// show how long it tried, not read like an instant error.
fn failure_summary(
    failure: &leveler_client_protocol::UiFailure,
    t: &crate::i18n::UiText,
) -> String {
    match failure.retries.filter(|n| *n > 0) {
        Some(n) => format!(
            "{} · {}",
            failure.summary,
            t.failure_retried.replace("{n}", &n.to_string())
        ),
        None => failure.summary.clone(),
    }
}

/// Muted machine subtitle for a failure: `provider · category_code` and the
/// HTTP status when there was one (`kimi · invalid_request · HTTP 400`). The
/// status is the first thing a provider report needs, so it rides on the
/// always-visible line rather than only the disclosure.
fn failure_subtitle(failure: &leveler_client_protocol::UiFailure) -> Option<String> {
    let mut parts = Vec::new();
    if let Some(provider) = failure.provider.as_deref().filter(|p| !p.is_empty()) {
        parts.push(provider.to_string());
    }
    parts.push(failure.category.code().to_string());
    if let Some(status) = failure.status {
        parts.push(format!("HTTP {status}"));
    }
    Some(parts.join(" · "))
}

/// The failure's disclosure text: every machine fact the runtime proved, one
/// per line, then the provider's own (already sanitized) reason.
///
/// Built here from the typed failure so the renderer never parses a provider
/// payload and raw request/response bodies are never dumped. Absent facts are
/// omitted rather than shown blank.
fn failure_detail_text(
    failure: &leveler_client_protocol::UiFailure,
    t: &crate::i18n::UiText,
) -> String {
    const LABEL_WIDTH: usize = 10;
    let mut lines: Vec<String> = Vec::new();
    let mut field = |label: &str, value: &str| {
        let value = value.trim();
        if !value.is_empty() {
            lines.push(format!(
                "{}  {value}",
                crate::render::pad_display(label, LABEL_WIDTH)
            ));
        }
    };
    field(
        t.failure_provider_label,
        failure.provider.as_deref().unwrap_or(""),
    );
    field(
        t.failure_model_label,
        failure.model.as_deref().unwrap_or(""),
    );
    field(t.failure_category_label, failure.category.code());
    if let Some(status) = failure.status {
        field(t.failure_http_label, &status.to_string());
    }
    field(
        t.failure_code_label,
        failure.provider_code.as_deref().unwrap_or(""),
    );
    field(
        t.failure_request_id_label,
        failure.request_id.as_deref().unwrap_or(""),
    );
    if let Some(n) = failure.retries.filter(|n| *n > 0) {
        field(t.failure_retries_label, &n.to_string());
    }
    field(t.failure_reason_label, &failure.detail);
    lines.join("\n")
}

fn turn_end_summary(state: &AppState, status: TurnEndStatus) -> Option<String> {
    let t = state.t();
    let mut parts = Vec::new();
    if let Some(n) = state.turn_diff_files
        && n > 0
    {
        parts.push(if n == 1 {
            t.summary_files_one.to_string()
        } else {
            t.summary_files_many.replace("{}", &n.to_string())
        });
    }
    // An Unverified turn's own label already says "未自动验证". A check count
    // beside it is a second authority on the same fact, and the line then
    // reads "not verified · verified 1/1".
    let label_states_verification = matches!(status, TurnEndStatus::Unverified);
    // Unverified / incomplete / failed / cancelled: no success verify mark.
    let allow_success_verify = matches!(
        status,
        TurnEndStatus::Completed | TurnEndStatus::CompletedWithWarnings | TurnEndStatus::Answered
    );
    // Open plan steps travel with the summary only while the turn can still be
    // continued — there "计划 5/9" is real progress information. On a finished
    // turn it would be a stale plan arguing against the outcome the runtime
    // just reported, which is two authorities on one line.
    let plan_open = state
        .plan
        .as_ref()
        .filter(|_| !work_is_finished(status))
        .and_then(|p| {
            let (k, n) = crate::workbench::plan_done_total(p);
            (n > 0 && k < n).then_some((k, n))
        });
    if let Some(v) = &state.turn_verification {
        if let Some(passed) = v.passed {
            if passed {
                if allow_success_verify {
                    // Prefer strict green: all listed checks Passed (and non-empty).
                    let all_passed = !v.checks.is_empty()
                        && v.checks
                            .iter()
                            .all(|c| c.status == leveler_client_protocol::CheckState::Passed);
                    parts.push(t.summary_verify_ok.to_string());
                    // Verification passed, so a check that failed did not gate
                    // it: say which, and that it does not block.
                    let advisory: Vec<&str> = v
                        .checks
                        .iter()
                        .filter(|c| c.status == leveler_client_protocol::CheckState::Failed)
                        .map(|c| c.name.as_str())
                        .collect();
                    if !all_passed && !advisory.is_empty() {
                        parts.push(
                            t.summary_verify_advisory_failed
                                .replace("{}", &advisory.join("、")),
                        );
                    }
                }
                // else: Unverified turn — omit success chrome entirely
            } else {
                parts.push(t.summary_verify_failed.to_string());
            }
        } else if !v.checks.is_empty() && !label_states_verification {
            let ok = v
                .checks
                .iter()
                .filter(|c| c.status == leveler_client_protocol::CheckState::Passed)
                .count();
            parts.push(
                t.summary_verify_partial
                    .replacen("{}", &ok.to_string(), 1)
                    .replacen("{}", &v.checks.len().to_string(), 1),
            );
        }
    }
    if let Some((k, n)) = plan_open {
        parts.push(t.summary_plan.replacen("{}", &k.to_string(), 1).replacen(
            "{}",
            &n.to_string(),
            1,
        ));
    }
    if parts.is_empty() {
        None
    } else {
        Some(parts.join(" · "))
    }
}

/// The activity label and the clock that belongs to it are one fact; clearing
/// half of it would leave a stale command elapsed on the status line.
fn clear_activity(state: &mut AppState) {
    state.activity = None;
    state.activity_elapsed_secs = None;
    state.reconnecting = None;
    state.reconnected_until = None;
}

fn replace_active_background_tasks(
    state: &mut AppState,
    tasks: &[leveler_client_protocol::UiActiveBackgroundTask],
) {
    let still_active = |id: &str| tasks.iter().any(|task| task.task_id == id);
    if matches!(&state.activity_selected, Some(crate::activity::ActivityId::Background(id)) if !still_active(id))
    {
        state.activity_selected = None;
    }
    if matches!(&state.activity_open, Some(crate::activity::ActivityId::Background(id)) if !still_active(id))
    {
        state.activity_open = None;
    }
    state.background_task_labels.clear();
    for task in tasks {
        let label = crate::render::tool_summary_for(
            "run_command",
            &serde_json::json!({ "program": task.program, "args": task.args }).to_string(),
            state.t(),
        );
        let label = if label.is_empty() {
            task.program.clone()
        } else {
            label
        };
        state.background_task_labels.insert(
            task.task_id.clone(),
            crate::state::BackgroundTaskChrome::running(
                label,
                state.elapsed_secs.saturating_sub(task.elapsed_ms / 1000),
            ),
        );
    }
}

fn archive_active_plan(state: &mut AppState) {
    if let Some(plan) = state.plan.take() {
        state.transcript.push_plan(plan);
    }
}

pub(super) fn start_turn(state: &mut AppState) {
    // A new turn owns the current activity view. The previous turn's settled
    // children are history now — the transcript kept them — and must not keep
    // rendering as work in flight. A child still open at the boundary stays:
    // the runtime continues or settles it in a later turn.
    state.team.retire_settled(state.elapsed_secs);
    state.turn_tool_calls = 0;
    state.status = RuntimeStatus::Busy;
    state.finalization_stage = None;
    state.project_rule_sources.clear();
    // The previous turn's next step is spent — a new turn is under way.
    crate::suggestion::clear(state);
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

/// Rebuild the transcript from a session's durable history. Each entry goes
/// through the same projection a live event does, in a scratch state, so the
/// replay cannot move this session's status, plan, roster or notifications;
/// only the rebuilt transcript is kept.
fn replay_history(
    state: &mut AppState,
    entries: Vec<leveler_client_protocol::UiHistoryEntry>,
    omitted_turns: u32,
) {
    let mut scratch = AppState::new(
        crate::theme::Theme::no_color(),
        crate::state::Boot {
            session_id: state.session_id.clone(),
            user: String::new(),
            version: String::new(),
            show_welcome: false,
            draft_path: None,
            history_path: None,
            context_window: 0,
            locale: state.locale,
            untrusted_config: Vec::new(),
            reasoning_effort: None,
        },
    );
    scratch.size = state.size;
    if omitted_turns > 0 {
        scratch.transcript.push_note(
            state
                .t()
                .history_omitted_turns
                .replace("{}", &omitted_turns.to_string()),
        );
    }
    for entry in entries {
        if entry.turn_start {
            start_turn(&mut scratch);
        }
        scratch.elapsed_secs = entry.turn_elapsed_ms / 1000;
        apply_runtime(&mut scratch, entry.event);
    }
    // A turn the log never closed (its runtime died) has no outcome to show.
    scratch.transcript.finalize_in_flight();
    state.transcript.replace_with(scratch.transcript);
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

/// What a snapshot is allowed to do to the view's transcript.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum TranscriptAdoption {
    /// The snapshot is the view: opening a session, or resyncing after events
    /// were missed. Its messages replace what is on screen.
    Replace,
    /// The snapshot only confirms a command this client sent. Its messages are
    /// conversation text and carry no tool calls, so rebuilding from them would
    /// erase every tool row the session has shown — one wipe per message sent.
    KeepLive,
}

/// Apply a snapshot that only confirms a command (a delivered submission, a
/// rejected one): runtime truth for status and pending work, but never a
/// reason to throw away the live transcript.
pub(super) fn apply_session_confirmation(state: &mut AppState, session: UiSessionSnapshot) {
    apply_session_with(state, session, TranscriptAdoption::KeepLive);
}

fn apply_session(state: &mut AppState, session: UiSessionSnapshot) {
    apply_session_with(state, session, TranscriptAdoption::Replace);
}

fn apply_session_with(
    state: &mut AppState,
    session: UiSessionSnapshot,
    adoption: TranscriptAdoption,
) {
    // Only a switch to a DIFFERENT session resets per-session view state; a
    // same-session resync (e.g. after a broadcast lag) must keep live plan/diff/
    // token state intact.
    let switching = state.session_id != session.id;
    apply_meta(state, &session);
    // Snapshot is runtime truth. A pending Shift+Tab from the previous
    // connection must not keep driving the cycle, and a next-step ghost from
    // the transcript this snapshot is about to replace no longer applies.
    state.pending_permission = None;
    crate::suggestion::clear(state);
    state.status = match session.status.as_str() {
        "running" => RuntimeStatus::Busy,
        "failed" => RuntimeStatus::Error,
        _ => RuntimeStatus::Idle,
    };
    state.finalization_stage = if state.status == RuntimeStatus::Busy {
        session.finalization_stage
    } else {
        None
    };
    // A turn input this client sent and the runtime has not answered is work
    // still owed. A snapshot taken before it landed must not reopen the
    // composer for a second one — that is how a retry becomes a second turn.
    if state.status == RuntimeStatus::Idle
        && state
            .pending_submissions
            .iter()
            .any(|pending| pending.command.session_id() == Some(&session.id))
    {
        state.status = RuntimeStatus::Busy;
    }

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

    // Snapshot `plan` is live-view state. Adopt it only while the runtime says
    // this session is actually running; idle/terminal history is rebuilt from
    // durable events below, through the same reducer as the live stream.
    state.plan = (state.status == RuntimeStatus::Busy)
        .then_some(session.plan)
        .flatten();
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
        clear_activity(state);
        state.finalization_stage = None;
        state.turn_tool_calls = 0;
        state.screen_scroll = 0;
        state.pending_attachments.clear();
        state.command_selected = None;
        // Another session's children are not this session's.
        state.team = crate::multi_agent::TaskTeamView::default();
        // A side thread is scoped to the run it observed: leaving it behind
        // would let a follow-up reference a conversation this session never
        // had. Return to Main and drop it (the main draft is swapped back in).
        let template = state.image_token_template();
        if state.surface == crate::btw::SurfaceFocus::Btw {
            super::leave_btw(state);
        }
        state.btw = crate::btw::BtwThread::default();
        state.btw.draft.set_image_token_template(&template);
    }
    // A reconnect snapshot replaces (rather than merges) the active process
    // projection. Its entries come from the process registry, so terminal
    // records retained by the registry or replayed transcript events cannot
    // resurrect here.
    replace_active_background_tasks(state, &session.active_background_tasks);
    state.team.restore(&session.children, state.elapsed_secs);

    // A confirmation keeps the view it confirmed; only an empty one has
    // nothing to lose and takes the snapshot's text.
    if adoption == TranscriptAdoption::KeepLive && !switching && !state.transcript.is_empty() {
        return;
    }

    // Rebuild the transcript from the session's persisted messages. Opening a
    // different session (or a lagged resync) replaces the current view.
    state.transcript.clear();
    // Any command row focus referred to a call in the transcript just dropped.
    state.command_selected = None;
    // Durable goal recaps (long-goal P3) interleave at the transcript
    // position their checkpoint represents: messages `[0..ordinal)` precede
    // the recap. A recap without a usable position lands after the messages
    // — still history, never dropped.
    let mut recaps = session.recaps.iter().cloned().peekable();
    for message in &session.messages {
        while let Some(next) = recaps.peek() {
            let due = matches!(
                (next.transcript_ordinal, message.ordinal),
                (Some(recap_at), Some(message_at)) if recap_at <= message_at
            );
            if !due {
                break;
            }
            let recap = recaps.next().expect("peeked");
            state.transcript.push_goal_recap(recap);
        }
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
            // Written by the runtime into the model's context (a child's
            // settlement, a lost or resumed delegation), not typed by the user.
            UiRole::User
                if message.kind == Some(leveler_client_protocol::UiMessageKind::RuntimeNotice) =>
            {
                state.transcript.push_note(message.text.clone());
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
    for recap in recaps {
        state.transcript.push_goal_recap(recap);
    }
    // Replayed history carries no tool-call ordering, so the live classifier
    // never fires for it. Decide it from the turn shape instead, or every
    // restored answer would render as undecided (and therefore bounded) prose.
    state.transcript.classify_replayed_history();
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
        // Back-dated by the runtime's own clock, so a reconnect does not
        // restart a long command at zero.
        let started = state.elapsed_secs as i64 - (tool.elapsed_ms / 1000) as i64;
        let id = tool.id.clone();
        state
            .transcript
            .push_tool_started(tool.id, tool.name, tool.arguments, false, started);
        state.transcript.append_tool_output(&id, &tool.output_tail);
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
