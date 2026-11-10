use leveler_client_protocol::{
    NotificationLevel, PermissionProfile, RuntimeEvent, RuntimeStatus, UiInputState, UiPlanState,
    UiRole, UiSessionSnapshot,
};

use crate::overlay::Overlay;
use crate::overlay::approval::ApprovalOverlay;
use crate::overlay::clarification::ClarificationOverlay;
use crate::state::{AppState, Notification, PendingInteraction};
use crate::transcript::{InputDelivery, TurnEndStatus};

/// Record which build is serving this client. Only two KNOWN identities can
/// disagree: an unknown one proves nothing and must neither raise nor clear a
/// version anomaly, so the last confirmed answer stands.
fn observe_runtime_build(state: &mut AppState, build: &leveler_client_protocol::BuildIdentity) {
    let expected = leveler_client_protocol::BuildIdentity::current();
    if build.is_known() && expected.is_known() {
        state.runtime_build = Some(build.clone());
        state.runtime_version_mismatch = !expected.matches(build);
    }
}

/// A previously shown version warning is no longer true once a matching build
/// answers. Only that exact notice is dismissed; another risk on the row stays.
fn clear_resolved_version_notice(state: &mut AppState) {
    if !state.runtime_version_mismatch
        && state.notification.as_ref().is_some_and(|notice| {
            notice.level == NotificationLevel::Warning
                && notice.message == state.t().runtime_version_mismatch
        })
    {
        state.notification = None;
    }
}

pub(super) fn apply_runtime(state: &mut AppState, event: RuntimeEvent) {
    match event {
        RuntimeEvent::GlobalTasksLoaded { .. } => {}
        RuntimeEvent::RuntimeReady => {}
        RuntimeEvent::RuntimeBuildObserved { runtime_build } => {
            let was_mismatched = state.runtime_version_mismatch;
            observe_runtime_build(state, &runtime_build);
            clear_resolved_version_notice(state);
            // A session subscription re-observes the same build. Only the
            // transition earns a new warning; the issue row is the durable
            // reminder, and repeats must not reset its clock.
            if state.runtime_version_mismatch && !was_mismatched {
                let message = state.t().runtime_version_mismatch.to_string();
                state.notification = Some(Notification {
                    level: NotificationLevel::Warning,
                    message,
                });
            }
        }
        RuntimeEvent::RuntimeReconnected {
            runtime_build,
            runtime_restarted,
        } => {
            // The transport reports facts; the wording is decided here, where
            // the turn state lives. Only work that was actually at risk earns
            // a warning: a turn must have been running, and the process must
            // have been replaced or been unverifiable. `Some(false)` proves
            // the process never changed, so its running work is untouched.
            let t = state.t();
            observe_runtime_build(state, &runtime_build);
            clear_resolved_version_notice(state);
            let work_at_risk = if state.is_busy() {
                match runtime_restarted {
                    Some(false) => None,
                    Some(true) => Some((
                        NotificationLevel::Warning,
                        t.reconnect_work_may_have_stopped,
                    )),
                    None => Some((NotificationLevel::Warning, t.reconnect_work_state_unknown)),
                }
            } else {
                None
            };
            // A different KNOWN build means this client is still served by a
            // generation other than the one installed — only a restart
            // switches. Unknown proves nothing, so it is not a version change.
            // The mismatch itself lives on the issue row; this notice is the
            // one-time alert and may be outranked by work that was at risk.
            let version_changed = state
                .runtime_version_mismatch
                .then_some((NotificationLevel::Warning, t.runtime_version_mismatch));
            if let Some((level, message)) = work_at_risk.or(version_changed) {
                state.notification = Some(Notification {
                    level,
                    message: message.to_string(),
                });
            }
        }
        RuntimeEvent::SessionOpened { session } => apply_session(state, session),
        // The runtime states what it closed; the snapshot that follows is the
        // authority for the session's new state. The count is the runtime's
        // own fact, so the copy can name it without guessing.
        RuntimeEvent::CrashWindowAcknowledged { closed, .. } => {
            let message = state
                .t()
                .recovery_acknowledged
                .replacen("{}", &closed.to_string(), 1);
            state.notification = Some(Notification {
                level: NotificationLevel::Info,
                message,
            });
        }
        // The stream is back but the session could not be re-read. The view may
        // still show pre-break state, so say so; input stays locked because no
        // snapshot has re-established authority. The transport logs the raw
        // failure; it is diagnostic, not user copy.
        RuntimeEvent::SessionResyncFailed { error } => {
            tracing::warn!(%error, "session resync failed after reconnect");
            let message = state.t().reconnect_resync_failed.to_string();
            state.notification = Some(Notification {
                level: NotificationLevel::Error,
                message,
            });
        }
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
                && !state.transcript.has_running_user_shell()
                && !entries.is_empty()
            {
                replay_history(state, entries, omitted_turns);
            }
        }
        RuntimeEvent::SessionUpdated { session } => {
            let previous = state.mode;
            apply_meta(state, &session);
            // A turn terminal that does not settle the goal republishes this
            // row: the goal is still owed, and the pause the user set is still
            // theirs. Reading it here is what keeps the badge honest across an
            // ordinary question in a Chat session.
            adopt_execution_task(state, session.execution_task.clone());
            // The runtime's axis is the route the NEXT turn takes. A session the
            // runtime returned to Chat is not running a Goal turn, so the mode
            // chip must not keep claiming one after a goal lifecycle ended.
            if !state.collaboration.eq_ignore_ascii_case("goal") {
                state.goal_mode_active = false;
            }
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
        RuntimeEvent::AttachmentAdded { attachment, .. } => {
            // Staging an attachment is the start of a new message, however the
            // user got here (clipboard image, `/image`, `/attach`). The last
            // turn's next step no longer describes what they are composing.
            crate::suggestion::dismiss(state);
            // The image goes where the user is writing, as `[图片 #N]`, and
            // takes the place among the staged images that its token has in
            // the sentence — paste one ahead of another and it IS the first.
            let index = state.composer.insert_image_token();
            let index = index.min(state.pending_attachments.len());
            state.pending_attachments.insert(index, attachment);
        }
        RuntimeEvent::AttachmentProcessingFailed { error, .. } => {
            state.notification = Some(Notification {
                level: NotificationLevel::Error,
                message: format!("附件处理失败: {error}"),
            });
        }
        RuntimeEvent::UserMessageAdded { message } => {
            let kind = message.kind;
            let shown = state.message_with_images(message.images, &message.text);
            match kind {
                // The runtime authored this into the model's context on the user
                // transport role; the person did not write it. It gets the
                // runtime-notice presentation, never the user author bar — and
                // it is never reasoning, so it is never a Thought.
                Some(leveler_client_protocol::UiMessageKind::RuntimeNotice) => {
                    if !shown.trim().is_empty() {
                        state.transcript.push_note(shown);
                    }
                }
                // An accepted input carries a durable identity the whole
                // lifecycle shares. Painting by that identity — never by text —
                // is what keeps a retried or replayed announcement one row.
                Some(leveler_client_protocol::UiMessageKind::PendingInput) => {
                    state
                        .transcript
                        .push_input(message.id, shown, InputDelivery::Pending);
                }
                Some(leveler_client_protocol::UiMessageKind::DeliveredInput) => {
                    state
                        .transcript
                        .push_input(message.id, shown, InputDelivery::Delivered);
                }
                Some(leveler_client_protocol::UiMessageKind::CancelledInput) => {
                    state.transcript.push_input(
                        message.id,
                        shown,
                        InputDelivery::Cancelled(String::new()),
                    );
                }
                None => state.transcript.push_user_if_new(shown),
            }
        }
        RuntimeEvent::InputAdmitted { message } => {
            // Admission and consumption can be emitted by different runtime
            // threads. A late admission cannot undo a terminal journal fact;
            // an unknown submission can still become confirmed as pending.
            if !matches!(
                state.transcript.input_state(&message.id),
                Some(InputDelivery::Delivered | InputDelivery::Cancelled(_))
            ) {
                let shown = state.message_with_images(message.images, &message.text);
                state
                    .transcript
                    .push_input(message.id, shown, InputDelivery::Pending);
            }
        }
        RuntimeEvent::InputSettled {
            message_id,
            state: settled,
            reason,
            ..
        } => {
            // A live client already drew this input at admission. A client that
            // subscribed later learns the text from the snapshot; this event is
            // then only a state refinement for a row it does not have, and the
            // snapshot remains the authority.
            match settled {
                UiInputState::Delivered => {
                    state
                        .transcript
                        .set_input_state(&message_id, InputDelivery::Delivered);
                }
                UiInputState::Cancelled => {
                    state
                        .transcript
                        .set_input_state(&message_id, InputDelivery::Cancelled(reason));
                }
            }
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
            // The previous attempt's partial reasoning is spent with the deltas
            // it arrived in: a retry is not a Thought.
            state.transcript.reset_thought();
        }
        RuntimeEvent::AssistantTextDelta { message_id, delta } => {
            mark_turn_busy(state);
            state.transcript.append_assistant(&message_id, &delta);
        }
        RuntimeEvent::ReasoningStarted => {
            mark_turn_busy(state);
            state.transcript.begin_thought();
        }
        RuntimeEvent::ReasoningDelta { delta } => {
            mark_turn_busy(state);
            // The ONE mutable reasoning block: every delta grows the same
            // transcript item in place, so a stream never appends Thought after
            // Thought.
            state.transcript.append_thought(&delta);
        }
        RuntimeEvent::ReasoningCompleted { elapsed_ms } => {
            mark_turn_busy(state);
            // Freeze the live block with the runtime's own measurement. The
            // text stays on screen as immutable history from here on.
            state.transcript.finish_thought(Some(elapsed_ms));
        }
        RuntimeEvent::AssistantMessageCompleted { message_id } => {
            state.transcript.finish_assistant(&message_id);
        }
        RuntimeEvent::DeliveryCommitted { text } => {
            // Settlement can follow the real answer with a short progress
            // note. Name the runtime's delivery before the turn-end classifier
            // treats that note as the answer. No new transcript row: the text
            // is already an assistant message.
            state.transcript.commit_named_answer(&text);
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
        RuntimeEvent::CommandProgress { elapsed_ms, .. } => {
            mark_turn_busy(state);
            state.activity = Some(state.t().command_activity_running.to_string());
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
            model_step,
            answer_effect,
        } => {
            mark_turn_busy(state);
            state.turn_tool_calls = state.turn_tool_calls.saturating_add(1);
            // Acting on the thought ends the reasoning segment: freeze what
            // arrived. A clean completion already froze it; an interrupted one
            // is marked, not claimed as finished.
            state.transcript.finish_thought(None);
            // Commands have their full identity in the conversation row;
            // the footer reports their running state and clock once.
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
            state.activity = Some(
                if crate::tool_taxonomy::activity_class(&name)
                    == crate::tool_taxonomy::ActivityClass::Shell
                {
                    state.t().command_activity_running.to_string()
                } else if target.is_empty() || target == "{}" {
                    verb
                } else {
                    format!("{verb} {target}")
                },
            );
            let started = state.elapsed_secs;
            state.transcript.push_tool_started(
                id,
                name,
                arguments,
                parallel,
                started as i64,
                model_step,
                answer_effect,
            );
        }
        RuntimeEvent::ToolTimingUpdated { id, timing } => {
            state
                .transcript
                .set_tool_timing(&id, Some(timing), Some(state.elapsed_secs));
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
            // A successful `commit_task` turns this Chat turn into an
            // executing task. The runtime's tool result is the authority:
            // a rejected commit must not change what the user sees.
            let task_committed = ok
                && state
                    .transcript
                    .tool_call_facts(&id)
                    .is_some_and(|(name, _)| name == "commit_task");
            state.transcript.complete_command(
                &id,
                ok,
                preview,
                duration_ms,
                applied_diff,
                exit_code,
                stop,
            );
            if task_committed {
                state.goal_mode_active = true;
                state.execution_task_active = true;
            }
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
        RuntimeEvent::PlanUpdated {
            plan,
            created,
            call_id,
        } => {
            // This is the runtime's latest declaration for the active turn.
            // Rendering may hide the indicator (a settled plan outside a
            // running turn), but the reducer keeps the exact value until the
            // terminal transition archives it.
            if let Some(call_id) = call_id {
                state.transcript.set_plan_fact(
                    &leveler_client_protocol::ToolCallId::new(call_id),
                    created,
                    plan.steps.len(),
                );
            }
            if !crate::workbench::plan_indicator_should_show(&plan, state.is_busy())
                && state.workbench_focus == crate::state::WorkbenchFocus::Plan
            {
                state.workbench_focus = crate::state::WorkbenchFocus::Input;
            }
            state.plan = Some(plan);
        }
        RuntimeEvent::DiffUpdated { query_id, diff } => {
            if let Some(query_id) = query_id {
                if state.diff_query.as_ref() != Some(&query_id) {
                    return;
                }
                state.diff_query = None;
            }
            state.turn_diff_files = Some(diff.files.len());
            state.diff_pending = false;
            state.diff_error = None;
            if state.diff_selected >= diff.files.len() {
                state.diff_selected = 0;
            }
            state.diff = Some(diff);
        }
        RuntimeEvent::DiffFailed { query_id, message } => {
            if let Some(query_id) = query_id {
                if state.diff_query.as_ref() != Some(&query_id) {
                    return;
                }
                state.diff_query = None;
            }
            // A failed read is not an empty workspace: keep the failure fact and
            // drop any stale listing so the screen cannot show old files as the
            // answer to a request that did not succeed.
            state.diff_pending = false;
            state.diff = None;
            state.diff_selected = 0;
            state.diff_error = Some(message);
        }
        RuntimeEvent::SessionCompleted { report } => {
            state.transcript.push_completion(report);
        }
        RuntimeEvent::PromptSuggestion { text } => {
            crate::suggestion::offer_generated(state, &text);
        }
        RuntimeEvent::AwaySummary { text } => {
            // A draft in the composer is the next message, not a reason to
            // hide the summary of the turn that just finished. A new turn is.
            if state.away_summary_pending && !state.is_busy() {
                state.transcript.push_away_summary(text);
            }
            state.away_summary_pending = false;
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
        // `RuntimeEvent::ContextUpdated` carried the old ContextCompiler's
        // candidate files and token guess. Nothing produces it any more, and the
        // gauge takes its number from the runtime's snapshot, so the variant is
        // kept for wire compatibility while it is ignored here.
        RuntimeEvent::ConfigReloaded {
            query_id,
            outcome,
            version,
            error,
            provider: _,
            model,
            thinking,
            changed,
            deferred_to_next_turn,
            restart_required,
        } => {
            let explicit = query_id.is_some();
            let touches_model = changed.iter().any(|dimension| {
                matches!(dimension.as_str(), "default_model" | "models" | "thinking")
            });
            // An automatic reload that changed only providers, tools or UI
            // policy has nothing the user must read; a model change or an
            // explicit `/reload` does.
            if !explicit && outcome == "applied" && !touches_model {
                return;
            }
            let t = state.t();
            let version = version.to_string();
            let notice = match outcome.as_str() {
                "applied" => {
                    let mut message = t.config_reloaded.replacen("{}", &version, 1);
                    if let Some(model) = model.as_deref() {
                        message.push_str(&format!(" · {model}"));
                    }
                    if let Some(thinking) = thinking.as_deref() {
                        message.push_str(&format!(" · {thinking}"));
                    }
                    if deferred_to_next_turn {
                        message.push_str(" · ");
                        message.push_str(t.config_next_turn);
                    }
                    if !restart_required.is_empty() {
                        message.push(' ');
                        message.push_str(t.config_restart_required);
                        let named: Vec<&str> = restart_required
                            .iter()
                            .map(|component| match component.as_str() {
                                "ui_language" => t.config_restart_ui,
                                "browser" => t.config_restart_browser,
                                other => other,
                            })
                            .collect();
                        message.push_str(&named.join(", "));
                    }
                    Notification {
                        level: NotificationLevel::Info,
                        message,
                    }
                }
                "unchanged" => Notification {
                    level: NotificationLevel::Info,
                    message: t.config_unchanged.to_string(),
                },
                _ => Notification {
                    level: NotificationLevel::Warning,
                    message: t
                        .config_reload_failed
                        .replacen("{}", error.as_deref().unwrap_or("unknown"), 1)
                        .replacen("{}", &version, 1),
                },
            };
            state.notification = Some(notice);
        }
        RuntimeEvent::ContextUpdated { .. } => {}
        RuntimeEvent::TokenUsage {
            input_tokens,
            output_tokens,
            cached_input_tokens,
            // Reasoning is a breakdown already inside `output_tokens`; a gauge
            // of the window in use has nothing to add for it.
            ..
        } => {
            // Ignore all-zero reports so a missing provider usage chunk cannot
            // wipe a previous good reading (or a transcript estimate).
            if input_tokens == 0 && output_tokens == 0 {
                return;
            }
            state.token_input = input_tokens;
            state.token_output = output_tokens;
            state.token_cached = cached_input_tokens;
            // These describe the request that JUST finished (`↑` prompt, `↓`
            // output). They are deliberately not copied into the window gauge:
            // a finished request is not the next request's projection, and the
            // gauge's own number comes from the runtime's snapshot. The turn
            // receipt is not built from them either — the runtime settles that
            // once from its durable model-call ledger (`RuntimeEvent::TurnUsage`).
        }
        RuntimeEvent::TurnUsage { usage } => {
            // The settled projection of this turn's durable `model_requests`
            // rows. The bridge sends it just before the display terminal, so
            // the receipt normally picks it up from `settled_turn_usage` when
            // that terminal is frozen. The transcript patch below covers a
            // replay or an out-of-order delivery where the terminal came first.
            state.settled_turn_usage = Some(usage.clone());
            state.transcript.settle_last_turn_receipt(&usage);
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
        RuntimeEvent::TurnCompletedWithWarnings {
            reason,
            unregistered_task,
        } => {
            let status = if unregistered_task {
                TurnEndStatus::AnsweredUnregistered
            } else {
                TurnEndStatus::CompletedWithWarnings
            };
            finish_turn(state, status, Some(reason));
        }
        RuntimeEvent::TurnAnswered => {
            finish_turn(state, TurnEndStatus::Answered, None);
        }
        RuntimeEvent::TurnPaused { reason } => {
            // The model answered inside the range the user authorized. What
            // differs is the GOAL lifecycle: it still owes the withheld stage,
            // so the header must say paused, not completed, and the work stays
            // resumable. The plan strip already names the boundary from the
            // user's own instruction.
            finish_turn(state, TurnEndStatus::Answered, None);
            if let Some(goal) = state.active_goal.as_mut() {
                goal.pause(std::time::Instant::now());
            }
            state.execution_task_active = true;
            state.resumable_task = true;
            state.notification = Some(Notification {
                level: NotificationLevel::Info,
                message: reason,
            });
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
        RuntimeEvent::TurnBlocked { reason } => {
            // A blocked goal is not an ordinary incomplete turn: the runtime
            // left the goal unsettled, so the work already done stands and the
            // same goal can be re-entered. `reason` is the model's declared
            // blocking summary, carried for display only.
            finish_turn(state, TurnEndStatus::Blocked, Some(reason));
            state.notification = None;
        }
        RuntimeEvent::TurnFailed {
            error,
            failure,
            recovery,
        } => {
            note_turn_settled(state);
            state.status = RuntimeStatus::Error;
            state.finalization_stage = None;
            clear_activity(state);
            state.goal_mode_active = false;
            state.transcript.finalize_in_flight();
            state.team.mark_unreported_at_turn_end(state.elapsed_secs);
            state.cancel_armed = false;
            state.cancel_repeat_armed = false;
            seal_analysis_segment(state);
            // A recoverable provider failure is a continuation point, not a
            // dead task. The runtime re-checks before it resumes; this only
            // decides what the user is told.
            state.resumable_task = failure.as_ref().is_some_and(failure_is_resumable);
            // A resumable failure is a continuation point: show it as paused,
            // which is what `继续` can re-enter. A hard failure stays red.
            let resumable = state.resumable_task;
            // The turn that failed may not have been the goal's at all (a
            // question asked while the goal waits). The user's pause is still
            // the truth about the goal, so a turn failure never settles it.
            let paused_owed = owed_goal_is_paused(state);
            if let Some(goal) = state.active_goal.as_mut() {
                let now = std::time::Instant::now();
                if resumable || paused_owed {
                    goal.pause(now);
                } else {
                    goal.fail(now);
                }
            }
            // A hard failure settles the task; only a resumable failure — or a
            // pause the user set — keeps the owed-task badge.
            if !resumable && !paused_owed {
                state.execution_task_active = false;
            }
            // ONE terminal failure → ONE primary block. The turn-end marker
            // states the outcome but must not carry a second copy of the
            // failure text.
            let block = {
                let t = state.t();
                match (&failure, &recovery) {
                    // A recovery stop is not a tool failure: the interrupted
                    // call's outcome is unknown and the runtime will not guess.
                    // The next step is the user's, so state it instead of
                    // forwarding the engine's diagnostic.
                    (_, Some(stop)) => crate::transcript::FailureBlock {
                        display: crate::fold::DisplayMode::Collapsed,
                        title: t.recovery_title.to_string(),
                        summary: t.recovery_paused.to_string(),
                        subtitle: Some(t.recovery_next_step.to_string()),
                        // The raw engine message and the call evidence stay
                        // available for diagnosis; they are not the prompt.
                        detail: format!("{} · {}\n{}", stop.tool, stop.call_id, error),
                    },
                    (Some(f), None) => crate::transcript::FailureBlock {
                        display: crate::fold::DisplayMode::Collapsed,
                        title: t.failure_title.to_string(),
                        summary: failure_summary(f, t),
                        subtitle: failure_subtitle(
                            f,
                            state.resumable_task.then_some(t.resumable_hint),
                        ),
                        detail: failure_detail_text(f, t),
                    },
                    // A failure the runtime could not type (legacy event, or a
                    // non-provider failure): show the string as the summary.
                    (None, None) => crate::transcript::FailureBlock {
                        display: crate::fold::DisplayMode::Collapsed,
                        title: t.failure_title.to_string(),
                        summary: error.clone(),
                        subtitle: None,
                        detail: String::new(),
                    },
                }
            };
            state.transcript.push_failure(block);
            let summary = turn_end_summary(state, TurnEndStatus::Failed);
            archive_active_plan(state);
            push_turn_receipt(state, TurnEndStatus::Failed, summary, None);
            state.turn_diff_files = None;
        }
        RuntimeEvent::TurnCancelled => {
            note_turn_settled(state);
            state.status = RuntimeStatus::Idle;
            state.finalization_stage = None;
            clear_activity(state);
            state.goal_mode_active = false;
            state.transcript.finalize_in_flight();
            state.team.mark_unreported_at_turn_end(state.elapsed_secs);
            state.cancel_armed = false;
            state.cancel_repeat_armed = false;
            state.resumable_task = true;
            // An interrupted turn keeps its goal: the clock freezes and the
            // indicator waits for `继续` instead of disappearing.
            if let Some(goal) = state.active_goal.as_mut() {
                goal.pause(std::time::Instant::now());
            }
            let summary = turn_end_summary(state, TurnEndStatus::Cancelled);
            archive_active_plan(state);
            state.turn_diff_files = None;
            push_turn_receipt(state, TurnEndStatus::Cancelled, summary, None);
            seal_analysis_segment(state);
            state.notification = Some(Notification {
                level: NotificationLevel::Warning,
                message: state.t().cancelled_continue.to_string(),
            });
        }
        RuntimeEvent::TaskCancelled => {
            // The user cancelled the LOGICAL task. It is terminal: the resume
            // affordance is withdrawn so a `继续` cannot reopen it.
            note_turn_settled(state);
            state.status = RuntimeStatus::Idle;
            state.finalization_stage = None;
            clear_activity(state);
            state.goal_mode_active = false;
            state.execution_task_active = false;
            state.resumable_task = false;
            // The cancelled goal can never be continued; its durable
            // projection is gone with it.
            state.execution_goal = None;
            state.cancel_armed = false;
            state.cancel_repeat_armed = false;
            state.active_goal = None;
            state.notification = Some(Notification {
                level: NotificationLevel::Info,
                message: state.t().task_cancelled.to_string(),
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
            let display_title = title.clone();
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
                    &id,
                    &nickname,
                    ok,
                    detail,
                    projected,
                    stop,
                    limit,
                    state.elapsed_secs,
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
            // `detail` on a start event is the child's prompt. The conversation
            // shows the short title, and a terminal (title = None) must not
            // wipe the name recorded at spawn.
            state.transcript.set_sub_agent_title(&id, display_title);
        }
        // No TUI action issues this query. A response on the shared stream is
        // therefore necessarily owned by another client.
        RuntimeEvent::UnfinishedGoalsLoaded { .. } => {}
        // The schedule page's answer. The query id is the ownership check: a
        // response for a request this client did not send matches none of the
        // page's slots and is ignored, exactly like the agents queries above.
        RuntimeEvent::ScheduleManaged {
            query_id, response, ..
        } => {
            crate::schedules::apply_response(state, &query_id, *response);
        }
        RuntimeEvent::AgentsLoaded {
            query_id,
            agents,
            problems,
        } => {
            let ours = query_id.is_some() && query_id == state.agents_list_query;
            if !ours {
                return;
            }
            state.agents_list_query = None;
            let t = state.t();
            state
                .transcript
                .push_note(crate::agents_view::listing_note(&agents, &problems, t));
        }
        RuntimeEvent::AgentLoaded {
            query_id,
            name,
            agent,
            error,
        } => {
            let ours = query_id.is_some() && query_id == state.agent_detail_query;
            if !ours {
                return;
            }
            state.agent_detail_query = None;
            let note = match (agent, error) {
                (Some(detail), _) => crate::agents_view::detail_note(&detail, state.t()),
                (None, Some(error)) => error,
                (None, None) => format!("Agent \"{name}\" not found."),
            };
            state.transcript.push_note(note);
        }
        // The TUI has no agent mutation command. These acknowledgements belong
        // to the Web form that issued them, despite sharing the session stream.
        RuntimeEvent::AgentMutated { .. } => {}
        RuntimeEvent::GoalRecapCreated { recap } => {
            // History, never the lower runtime stack. Idempotent on
            // checkpoint_id inside push_goal_recap.
            state.transcript.push_goal_recap(recap);
        }
        RuntimeEvent::ChildContributionLoaded { query_id, detail } => {
            let ours = query_id.is_some() && query_id == state.child_contribution_query;
            if !ours {
                return;
            }
            state.child_contribution_query = None;
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
            preview,
            is_error,
        } => {
            // The activity projection and the transcript's one-word head are
            // fed the same event. `recent_step` keeps its compact wording; the
            // detail page reads the typed calls (tool, previews, status).
            let step = if phase == "tool_finished" {
                if is_error {
                    format!("{tool} ✗")
                } else {
                    format!("{tool} ✓")
                }
            } else {
                tool.clone()
            };
            state
                .team
                .apply_activity(&id, &phase, &tool, &preview, is_error);
            state.transcript.update_sub_agent_activity(&id, step);
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
                state.motion_clock.as_millis() as i64,
            );
            // The block is in the conversation now; its output and terminal
            // event update the same row in place.
        }
        RuntimeEvent::UserShellBackgrounded {
            execution_id,
            task_id,
        } => {
            state
                .transcript
                .background_user_shell(&execution_id, &task_id);
            // A background execution is no longer stoppable through the
            // foreground Command focus or Ctrl+C; its task list owns it now.
            if state.command_selected.as_ref()
                == Some(&crate::state::CommandTarget::UserShell(
                    execution_id.clone(),
                ))
            {
                state.command_selected = None;
                if state.workbench_focus == crate::state::WorkbenchFocus::Command {
                    state.workbench_focus = crate::state::WorkbenchFocus::Input;
                }
            }
        }
        RuntimeEvent::UserShellOutput {
            execution_id,
            stream,
            chunk,
        } => {
            // Sanitize BEFORE it enters any buffer: shell output is
            // untrusted terminal text (ANSI/OSC/control sequences). Carriage
            // returns are preserved (through a sentinel) so the renderer can
            // apply progress overwrites instead of showing one long line.
            let clean = crate::shell_render::sanitize_shell_output(&chunk);
            state
                .transcript
                .append_user_shell_output(&execution_id, &clean, stream == "stderr");
        }
        RuntimeEvent::UserShellOutputReplaced {
            execution_id,
            output_tail,
            output_truncated,
        } => {
            // The bounded live channel may have dropped chunks. The runtime's
            // retained view replaces the accumulated buffer; appending it
            // would keep every hole and duplicate the overlapping bytes.
            let clean = crate::shell_render::sanitize_shell_output(&output_tail);
            state
                .transcript
                .replace_user_shell_output(&execution_id, &clean, output_truncated);
        }
        RuntimeEvent::UserShellCaptureReady {
            execution_id,
            directory,
        } => {
            state
                .transcript
                .set_user_shell_full_output(&execution_id, Some(directory));
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
            // The Command focus exists to stop and open a RUNNING execution.
            // Once this one ends it is no longer a candidate, so the keys go
            // back to the composer instead of Enter toggling a settled row.
            if state.command_selected.as_ref()
                == Some(&crate::state::CommandTarget::UserShell(
                    execution_id.clone(),
                ))
            {
                state.command_selected = None;
                if state.workbench_focus == crate::state::WorkbenchFocus::Command {
                    state.workbench_focus = crate::state::WorkbenchFocus::Input;
                }
            }
        }
        RuntimeEvent::ContextCompacted { from, to } => {
            // The measured transcript was replaced. The runtime drops its own
            // accounting; the client must drop the snapshot AND the token
            // fallback so no pre-compaction figure survives the fold.
            state.context.loaded = None;
            state.context_tokens = 0;
            state.token_input = 0;
            state.token_output = 0;
            state.token_cached = 0;
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
        RuntimeEvent::BtwToolStarted { call_id, tool } => {
            state.btw.tool_started(call_id.to_string(), tool);
        }
        RuntimeEvent::BtwToolFinished {
            call_id,
            is_error,
            elapsed_ms,
            ..
        } => {
            state
                .btw
                .tool_finished(call_id.as_str(), is_error, elapsed_ms);
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
            pid,
        } => {
            // One lifecycle, one human label. The id stays as the projection
            // key (stable identity, never text matching); what the user reads
            // is the command it is running.
            let label = crate::tool_cell::command_detail(
                "run_command",
                &serde_json::json!({ "program": program, "args": args }).to_string(),
            )
            .unwrap_or_else(|| program.clone());
            // A repeated Started for the same id is a reconnect/replay fact,
            // not a new task: it refreshes the live entry silently. It never
            // turns a settled task back into a running one — a reused id is
            // an identity conflict, not a resurrection.
            match state.background_task_labels.get_mut(&task_id) {
                Some(chrome) => {
                    if chrome.is_running() {
                        chrome.pid = pid;
                        chrome.label = label;
                    }
                }
                None => {
                    state.background_task_labels.insert(
                        task_id.clone(),
                        crate::state::BackgroundTaskChrome {
                            pid,
                            ..crate::state::BackgroundTaskChrome::running(
                                label.clone(),
                                state.elapsed_secs,
                            )
                        },
                    );
                    state.notification = Some(Notification {
                        level: NotificationLevel::Info,
                        message: state.t().background_task_started.replace("{}", &label),
                    });
                }
            }
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
            // Only the answer to the query this view is waiting for. A
            // superseded query (the page re-asked while this one was in flight)
            // is dropped instead of overwriting the newer answer.
            if !state.context.track.accept(query_id.as_ref()) {
                return;
            }
            state.context.loaded = accounting;
            crate::context::clamp(&mut state.context);
        }
        RuntimeEvent::UsageLoaded { query_id, report } => {
            // Same correlation as the Context answer: a replay, another
            // client's answer, or an answer to a scope the user already left.
            if !state.usage.track.accept(query_id.as_ref()) {
                return;
            }
            if report.scope != state.usage.scope {
                // A `Tab` landed while this query was in flight. The answer is
                // for a scope that is no longer on screen, so it is dropped and
                // the page asks again for the scope now selected.
                state.usage.track.mark_dirty();
                return;
            }
            state.usage.loaded = Some(*report);
        }
        RuntimeEvent::ObservabilityLoaded {
            observation,
            query_id,
        } => {
            // Only a correlated current response for the query this view owns.
            // `query_id: None` is a legacy 1.5 payload — decode-safe, not owned.
            if !state.trace.track.accept(query_id.as_ref()) {
                return;
            }
            state.trace.loaded = Some(observation);
            state.trace.clamp();
        }
        RuntimeEvent::BackgroundTaskUnknown {
            task_id,
            duration_ms,
            output,
        } => {
            // The starter row is the only place the process's progress is
            // visible without opening the jobs page: an unprovable outcome has
            // to land there too, not just on the chrome.
            state.transcript.note_background_unknown(&task_id);
            let label = match state.background_task_labels.get_mut(&task_id) {
                Some(chrome) => {
                    chrome.ok = Some(false);
                    chrome.stopped = false;
                    chrome.unknown = true;
                    chrome.exit_code = None;
                    chrome.duration_ms = Some(duration_ms);
                    if !output.is_empty() {
                        chrome.output = output;
                    }
                    chrome.label.clone()
                }
                None => state.t().background_task_generic.to_string(),
            };
            crate::activity::bound_terminal_background(state);
            let linked_shell = state.transcript.items().iter().any(|item| {
                matches!(
                    item,
                    crate::transcript::TranscriptItem::UserShell(shell)
                        if shell.task_id.as_deref() == Some(task_id.as_str())
                )
            });
            if !linked_shell {
                state.notification = Some(Notification {
                    level: NotificationLevel::Warning,
                    message: state.t().background_task_unknown.replace("{}", &label),
                });
            }
        }
        RuntimeEvent::BackgroundTaskExited {
            task_id,
            exit_code,
            duration_ms,
            ok,
            stopped,
            output,
        } => {
            // The command row reads the starter preview. Activity chrome is
            // session-local and replay keeps only the transcript, so the exit
            // has to land on the call the row already shows.
            let has_starter = state.transcript.note_background_exit(
                &task_id,
                ok,
                stopped,
                exit_code,
                duration_ms,
            );
            // A user shell adopted into the background already has its own
            // record. That record is the user-facing completion; the generic
            // background notice would be a second surface for one fact.
            let linked_shell = state.transcript.items().iter().any(|item| {
                matches!(
                    item,
                    crate::transcript::TranscriptItem::UserShell(shell)
                        if shell.task_id.as_deref() == Some(task_id.as_str())
                )
            });
            let t = state.t();
            // Terminal is history, but the TUI keeps the entry so the Activity
            // row and its detail stay reopenable. The registry record is the
            // runtime's; this is only the view.
            let label = match state.background_task_labels.get_mut(&task_id) {
                Some(chrome) => {
                    chrome.ok = Some(ok);
                    chrome.stopped = stopped;
                    chrome.exit_code = exit_code;
                    chrome.duration_ms = Some(duration_ms);
                    // The final log is authoritative over the streamed chunks:
                    // a lifecycle broadcast lag could have dropped one.
                    if !output.is_empty() {
                        chrome.output = output;
                    }
                    chrome.label.clone()
                }
                // An exit for a task this session never saw start: report it in
                // the transcript, but do not invent a chrome entry with no label
                // or start clock for the Activity list.
                None => t.background_task_generic.to_string(),
            };
            crate::activity::bound_terminal_background(state);
            // Stopped is its own terminal outcome: a cancel is not a failure.
            let (message, level) = if ok {
                (
                    t.background_task_done.replace("{}", &label),
                    Some(NotificationLevel::Info),
                )
            } else if stopped {
                (
                    t.background_task_stopped.replace("{}", &label),
                    Some(NotificationLevel::Info),
                )
            } else {
                // A failure is not a transient notice: it becomes an ACTIVE
                // issue (the strip's issue cell and its page) and the task's
                // own row/output keep the evidence. A banner repeating the
                // command and exit code was the third surface for one fact.
                let failed = t.background_task_failed.replace("{}", &label);
                let message = match exit_code {
                    Some(code) => format!("{failed} · exit {code}"),
                    None => failed,
                };
                (message, None)
            };
            if !has_starter && !linked_shell {
                state.transcript.push_note(message.clone());
            }
            if !linked_shell && let Some(level) = level {
                state.notification = Some(Notification { level, message });
            }
        }
        RuntimeEvent::BackgroundTaskOutput { task_id, chunk } => {
            // Live tail from the runtime. Sanitize again (the client never
            // trusts raw terminal bytes) and bound the stored tail so a chatty
            // server cannot grow the projection without bound.
            if let Some(chrome) = state.background_task_labels.get_mut(&task_id) {
                let clean = leveler_core::sanitize_terminal_output(&chunk);
                if !clean.is_empty() {
                    // Unread is event evidence, independent of retained log
                    // length (which stops growing at the byte cap) or wrapping.
                    if let Some(view) = state.background_task_views.get_mut(&task_id)
                        && !view.follow
                    {
                        view.unread = view.unread.saturating_add(clean.lines().count());
                    }
                    chrome.output.push_str(&clean);
                    cap_background_output(&mut chrome.output);
                    chrome.last_output_at = Some(std::time::Instant::now());
                    // The command row that started the task shows the same live
                    // tail, so the user does not have to open the jobs page to
                    // see that the process is doing something. One runtime
                    // event, two projections of the same bytes.
                    state
                        .transcript
                        .append_background_output(&task_id, &clean, state.elapsed_secs);
                }
            }
        }
        RuntimeEvent::BackgroundTaskReady {
            task_id, evidence, ..
        } => {
            // Readiness is a RUNNING fact: the process kept going. It is stored
            // where every projection reads it and stamped onto the command row,
            // and never touches `ok` / `stopped`.
            let label = match state.background_task_labels.get_mut(&task_id) {
                Some(chrome) => {
                    if chrome.is_running() && chrome.ready.is_none() {
                        chrome.ready = Some(evidence.clone());
                    }
                    chrome.label.clone()
                }
                None => state.t().background_task_generic.to_string(),
            };
            state.transcript.note_background_ready(&task_id, &evidence);
            let message = state.t().background_task_ready.replace("{}", &label);
            state.notification = Some(Notification {
                level: NotificationLevel::Info,
                message,
            });
        }
        RuntimeEvent::BackgroundTasksReconciled { tasks } => {
            // This live correction names only what is still running. It
            // carries no durable record, so a vanished entry keeps an open
            // state; the next snapshot resolves it from the log.
            replace_active_background_tasks(state, &tasks, &[]);
        }
        RuntimeEvent::ForeignSessionList {
            tool,
            sessions,
            error,
        } => {
            // A late answer for a source the page is no longer showing belongs
            // to no one on screen; applying it would replace the list under
            // the reader.
            if state.foreign_sessions.tool != Some(tool) {
                return;
            }
            state.foreign_sessions.loading = false;
            state.foreign_sessions.sessions = sessions;
            state.foreign_sessions.error = error;
            state.foreign_sessions.selected = state
                .foreign_sessions
                .selected
                .min(state.foreign_sessions.sessions.len().saturating_sub(1));
        }
        RuntimeEvent::ForeignSessionImported {
            tool,
            native_id,
            warning,
            ..
        } => {
            state.foreign_sessions.importing = false;
            state.foreign_sessions.tool = None;
            state.foreign_sessions.sessions.clear();
            state.foreign_sessions.error = None;
            // The new native session owns the view; `SessionOpened` follows
            // with its snapshot.
            if state.active_screen == crate::screen::Screen::ForeignSessions {
                state.active_screen = crate::screen::Screen::Conversation;
                state.screen_scroll = 0;
            }
            // The runtime's warning prose is not this client's copy; the only
            // import warning is a source/workspace mismatch, and the exact
            // source directory is carried by the durable briefing.
            let t = state.t();
            let (level, template) = if warning.is_some() {
                (NotificationLevel::Warning, t.foreign_imported_mismatch)
            } else {
                (NotificationLevel::Info, t.foreign_imported)
            };
            let message = template
                .replacen("{}", tool.display_name(), 1)
                .replacen("{}", &native_id, 1);
            state.notification = Some(Notification { level, message });
        }
        RuntimeEvent::ForeignSessionImportFailed {
            tool,
            native_id,
            message,
        } => {
            // The picker stays: the source itself may still be importable, and
            // another row can be tried. Only the in-flight guard is released.
            state.foreign_sessions.importing = false;
            let text = state
                .t()
                .foreign_import_failed
                .replacen("{}", tool.display_name(), 1)
                .replacen("{}", &native_id, 1)
                .replacen("{}", &message, 1);
            state.notification = Some(Notification {
                level: NotificationLevel::Error,
                message: text,
            });
        }
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

/// Snapshot of the turn that is about to close. Every number is this turn's
/// own: the clock on screen, the steps and tools it ran, and the usage the
/// runtime booked for its requests. The footer window gauge is a different
/// measurement and is not copied here.
///
/// Token figures come only from the runtime's settled projection. A turn with
/// no settled projection reports none rather than a client-side sum of
/// transient per-request snapshots.
fn turn_receipt(state: &AppState) -> crate::transcript::TurnReceipt {
    let ended_hm = {
        let label = state.clock_label.trim();
        (!label.is_empty()).then(|| label.to_string())
    };
    let (input_tokens, output_tokens, cached_tokens) = state
        .settled_turn_usage
        .as_ref()
        .map(|usage| {
            (
                usage.input_tokens,
                usage.output_tokens,
                usage.cached_input_tokens.unwrap_or(0),
            )
        })
        .unwrap_or_default();
    crate::transcript::TurnReceipt {
        ended_hm,
        rounds: state.transcript.current_turn_rounds(),
        input_tokens,
        output_tokens,
        cached_tokens,
    }
}

/// A turn terminal has been applied. A command confirmation dispatched before
/// it carries a snapshot that cannot know the turn settled.
fn note_turn_settled(state: &mut AppState) {
    state.settled_turns = state.settled_turns.saturating_add(1);
}

fn finish_turn(state: &mut AppState, status: TurnEndStatus, detail: Option<String>) {
    note_turn_settled(state);
    // The runtime's own terminal says the goal lifecycle reached a successful
    // end. A missing final PROSE does not undo that: the acceptance gate ran
    // and passed. Keep the two facts separate instead of turning the goal
    // header red because the model closed with `update_goal` and no sentence.
    let goal_completed = matches!(
        status,
        TurnEndStatus::Completed | TurnEndStatus::CompletedWithWarnings
    );
    // A goal the user paused is still owed, and this turn answering an
    // ordinary question is not that goal's terminal: the runtime never ran it,
    // so the pause survives, and so does the badge that shows it.
    let paused_owed = owed_goal_is_paused(state);
    state.status = RuntimeStatus::Idle;
    state.finalization_stage = None;
    state.resumable_task = false;
    // An unregistered answer is not a Goal terminal. Keep the pause
    // resumable so the user can still continue the work the goal owes.
    if matches!(status, TurnEndStatus::AnsweredUnregistered) && paused_owed {
        state.resumable_task = true;
    }
    // A blocked goal is unsettled by the runtime's own rule: the work already
    // delivered stands and the same objective stays re-enterable. Withdrawing
    // continuation here would also turn `/goal cancel` into a local-only
    // clear, leaving the durable goal running.
    if matches!(status, TurnEndStatus::Blocked) {
        state.resumable_task = true;
    }
    clear_activity(state);
    state.goal_mode_active = false;
    state.transcript.finalize_in_flight();
    state.team.mark_unreported_at_turn_end(state.elapsed_secs);
    crate::suggestion::allow_generated(state);
    // §12: a clean outcome still needs an answer behind it. Only the two
    // outcomes that render as done are rewritten — an Incomplete or Failed turn
    // already says something went wrong.
    let answered = state.transcript.settle_final_answer();
    let unregistered = matches!(status, TurnEndStatus::AnsweredUnregistered);
    let status = match status {
        TurnEndStatus::Completed
        | TurnEndStatus::Answered
        | TurnEndStatus::AnsweredUnregistered
            if !answered =>
        {
            TurnEndStatus::NoFinalAnswer
        }
        other => other,
    };
    // A clean turn end settles the delegated task. Any other terminal keeps
    // it visible: the goal still owes work and `继续` can re-enter it. An
    // accepted goal is finished even when its closing prose is missing. A
    // paused goal is settled only by its own completion, never by a turn that
    // merely answered a question.
    let settles_the_goal = !paused_owed || goal_completed;
    if (work_is_finished(status) || goal_completed) && settles_the_goal {
        state.execution_task_active = false;
    }
    // Active Goal terminal. Done (green) fades on its own; a turn that stopped
    // short of done (amber) stays because it owes work; a failure the user may
    // have to act on (red) never hides on a timer.
    if let Some(goal) = state.active_goal.as_mut() {
        let now = std::time::Instant::now();
        if unregistered {
            // The refusal belongs to this answer, not to the Goal. Leave the
            // goal's phase untouched: a paused goal stays paused, a running
            // goal stays running, and an already-completed goal stays green.
        } else if goal_completed {
            goal.complete(now);
        } else if paused_owed {
            // The user's pause is still the last word about this goal, whatever
            // the unrelated turn that just ended concluded.
            goal.pause(now);
        } else {
            match status {
                TurnEndStatus::Answered => goal.complete(now),
                TurnEndStatus::Truncated | TurnEndStatus::Incomplete | TurnEndStatus::Cancelled => {
                    goal.pause(now)
                }
                // The goal is still owed; the blocked window stops the clock
                // but does not fail the goal.
                TurnEndStatus::Blocked => goal.pause(now),
                TurnEndStatus::NoFinalAnswer | TurnEndStatus::Failed => goal.fail(now),
                TurnEndStatus::AnsweredUnregistered => {}
                TurnEndStatus::Completed | TurnEndStatus::CompletedWithWarnings => {
                    goal.complete(now)
                }
            }
        }
    }
    state.cancel_armed = false;
    state.cancel_repeat_armed = false;
    // No usage, no reading. The gauge is hidden until the provider reports one
    // (`footer_context_usage` returns None for zero), because a number derived
    // from the visible transcript is not the window's occupancy: it ignores the
    // control context, the tool schema and reasoning, so it would understate the
    // real compaction pressure while looking authoritative.
    // Read BEFORE `push_turn_end`: a TurnEnd marker is the scan boundary, so
    // the handoff has to be taken while this turn is still the latest one.
    // The handoff is captured before the terminal so it owns only this turn.
    let handoff = state.transcript.latest_turn_handoff();
    // A blocked goal's next action is the runtime's continuation. The model's
    // own `next_step` is advice recorded in the recap; it is not the confirmed
    // resume action, so it must not become the offered input.
    let suggestion = match status {
        TurnEndStatus::Blocked => Some(state.t().suggestion_continue.to_string()),
        _ => handoff
            .as_ref()
            .and_then(|handoff| handoff.next_step.clone())
            .or_else(|| {
                (status == TurnEndStatus::Incomplete)
                    .then(|| state.t().suggestion_continue.to_string())
            }),
    };
    let summary = turn_end_summary(state, status);
    archive_active_plan(state);
    state.turn_diff_files = None;
    push_turn_receipt(state, status, summary, detail);
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
    // The reducer asks for a fresh recap after a successful turn. Clear any
    // acceptance window left by the previous one first.
    crate::away_summary::cancel(state);
}

/// Compact product summary for the turn-end marker.
/// Terminal states where the WORK itself is finished. Task outcome, plan
/// progress are separate facts: a finished task with a plan last reported at
/// 6/9 is entirely legal, and the outcome is the runtime's to report, not the
/// plan's to dispute.
fn work_is_finished(status: TurnEndStatus) -> bool {
    matches!(
        status,
        TurnEndStatus::Completed | TurnEndStatus::CompletedWithWarnings | TurnEndStatus::Answered
    )
}

/// Whether the session owes a goal the user paused.
fn owed_goal_is_paused(state: &AppState) -> bool {
    crate::active_goal::paused_owed(state)
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
fn failure_subtitle(
    failure: &leveler_client_protocol::UiFailure,
    resumable_hint: Option<&str>,
) -> Option<String> {
    let mut parts = Vec::new();
    if let Some(provider) = failure.provider.as_deref().filter(|p| !p.is_empty()) {
        parts.push(provider.to_string());
    }
    parts.push(failure.category.code().to_string());
    if let Some(status) = failure.status {
        parts.push(format!("HTTP {status}"));
    }
    if let Some(hint) = resumable_hint {
        parts.push(hint.to_string());
    }
    Some(parts.join(" · "))
}

/// Whether a typed failure is one a continuation may re-enter.
///
/// Mirrors the runtime's classification (`ModelErrorKind` → recoverable
/// provider/network faults) in product terms. It is a presentation mirror only.
fn failure_is_resumable(failure: &leveler_client_protocol::UiFailure) -> bool {
    matches!(
        failure.category,
        leveler_client_protocol::FailureCategory::Network
            | leveler_client_protocol::FailureCategory::Timeout
            | leveler_client_protocol::FailureCategory::Provider
            | leveler_client_protocol::FailureCategory::RateLimit
            // A local provider/model resolution failure is a configuration
            // problem, not a dead task: once the config is fixed `继续` can
            // re-enter. This is continuation, not automatic retry.
            | leveler_client_protocol::FailureCategory::LocalConfiguration
    )
}

/// The failure's inline detail: every machine fact the runtime proved, one
/// per line, then the provider's own reason. Values use the existing bounded
/// provider-text sanitizer before they become visible.
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
            let value = leveler_model::error::sanitize_provider_text(value);
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

/// Retained background-output cap. The runtime caps its own log too; the
/// client keeps a same-sized tail so a reconnect or a long-running server does
/// not grow the view model without bound.
const BACKGROUND_OUTPUT_CAP: usize = 64 * 1024;

fn cap_background_output(output: &mut String) {
    if output.len() <= BACKGROUND_OUTPUT_CAP {
        return;
    }
    let dropped = leveler_core::ceil_char_boundary(output, output.len() - BACKGROUND_OUTPUT_CAP);
    let marker = format!("\u{2026}[truncated {dropped} bytes]\u{2026}");
    *output = leveler_core::truncate_tail_bytes(output, BACKGROUND_OUTPUT_CAP, &marker);
}

fn background_label(program: &str, args: &[String]) -> String {
    crate::tool_cell::command_detail(
        "run_command",
        &serde_json::json!({ "program": program, "args": args }).to_string(),
    )
    .unwrap_or_else(|| program.to_string())
}

/// Apply a durable record's state to a chrome entry. Only the record's own
/// evidence is used: an absent terminal never becomes success or failure.
fn apply_background_record(
    chrome: &mut crate::state::BackgroundTaskChrome,
    record: &leveler_client_protocol::UiBackgroundTaskRecord,
) {
    use leveler_client_protocol::UiBackgroundTaskState as State;
    chrome.stopped = false;
    chrome.unknown = false;
    chrome.lost = false;
    chrome.conflict = false;
    match record.state {
        State::Running => chrome.ok = None,
        State::Completed => chrome.ok = Some(true),
        State::Failed => chrome.ok = Some(false),
        State::Cancelled => {
            chrome.ok = Some(false);
            chrome.stopped = true;
        }
        State::Lost => {
            chrome.ok = Some(false);
            chrome.lost = true;
        }
        State::Conflict => {
            chrome.ok = Some(false);
            chrome.conflict = true;
        }
        State::Unknown => {
            chrome.ok = Some(false);
            chrome.unknown = true;
        }
    }
    chrome.exit_code = record.exit_code;
    // A durable end freezes the clock. Without this, a task whose owner died
    // keeps accumulating time for as long as the view stays open.
    if record.state != State::Running
        && let Some(duration_ms) = record.duration_ms
    {
        chrome.duration_ms = Some(duration_ms);
    }
    if !record.output.is_empty() {
        chrome.output = record.output.clone();
        cap_background_output(&mut chrome.output);
    }
}

fn replace_active_background_tasks(
    state: &mut AppState,
    tasks: &[leveler_client_protocol::UiActiveBackgroundTask],
    records: &[leveler_client_protocol::UiBackgroundTaskRecord],
) {
    use leveler_client_protocol::UiBackgroundTaskState;
    let records_by_id: std::collections::HashMap<
        &str,
        &leveler_client_protocol::UiBackgroundTaskRecord,
    > = records
        .iter()
        .map(|record| (record.task_id.as_str(), record))
        .collect();
    let still_active = |id: &str| tasks.iter().any(|task| task.task_id == id);

    // A running entry the runtime no longer lists has ended somewhere. The
    // durable projection says how when it can prove it; otherwise the entry
    // stays with an open state instead of disappearing from view. Terminal
    // entries are the TUI's own retained history and stay.
    for (id, chrome) in state.background_task_labels.iter_mut() {
        if !chrome.is_running() || still_active(id) {
            continue;
        }
        match records_by_id.get(id.as_str()) {
            Some(record) => apply_background_record(chrome, record),
            None => {
                chrome.ok = Some(false);
                chrome.unknown = true;
            }
        }
    }
    for task in tasks {
        let label = background_label(&task.program, &task.args);
        // Carry the already-retained output across a reconcile: the runtime's
        // snapshot names the task, not its log, and re-inserting a blank entry
        // would silently drop everything the detail page had shown.
        let previous_output = state
            .background_task_labels
            .get(&task.task_id)
            .map(|chrome| chrome.output.clone())
            .unwrap_or_default();
        let mut chrome = crate::state::BackgroundTaskChrome::running(
            label,
            state.elapsed_secs.saturating_sub(task.elapsed_ms / 1000),
        );
        chrome.output = previous_output;
        chrome.pid = task.pid;
        // Readiness survives a reconnect because the runtime re-reports the
        // evidence it observed. It is never reconstructed from the command.
        if let Some(evidence) = task.ready_evidence.as_deref() {
            chrome.ready = Some(evidence.to_string());
            state
                .transcript
                .note_background_ready(&task.task_id, evidence);
        }
        // A live entry whose id also carries ambiguous durable history keeps
        // that history: a reused id is a conflict, never a fresh running task.
        if let Some(record) = records_by_id.get(task.task_id.as_str())
            && record.state != UiBackgroundTaskState::Running
        {
            apply_background_record(&mut chrome, record);
        }
        state
            .background_task_labels
            .insert(task.task_id.clone(), chrome);
    }
    // Restore the unresolved records this view never saw (a TUI started after
    // the runtime that hosted them ended). Terminal records are history the
    // live view already kept; they are not replayed as new activity.
    for record in records {
        if state.background_task_labels.contains_key(&record.task_id) {
            continue;
        }
        if matches!(
            record.state,
            UiBackgroundTaskState::Lost
                | UiBackgroundTaskState::Unknown
                | UiBackgroundTaskState::Conflict
        ) {
            let mut chrome = crate::state::BackgroundTaskChrome::running(
                background_label(&record.program, &record.args),
                state.elapsed_secs,
            );
            // No start was observed here, so the run time is unknown rather
            // than the zero a fabricated clock would imply.
            chrome.started_known = false;
            apply_background_record(&mut chrome, record);
            state
                .background_task_labels
                .insert(record.task_id.clone(), chrome);
        }
    }
    let known = |id: &str| state.background_task_labels.contains_key(id);
    if matches!(&state.activity_selected, Some(crate::activity::ActivityId::Background(id)) if !known(id))
    {
        state.activity_selected = None;
    }
    if matches!(&state.activity_open, Some(crate::activity::ActivityId::Background(id)) if !known(id))
    {
        state.activity_open = None;
    }
}

/// Settle command rows from the chrome's already-reconciled outcome.
///
/// A replayed `BackgroundTaskExited` event settles its own starter row. A task
/// that lost its owner, or whose terminal never reached the engine log, has no
/// such event; the snapshot's reconciled record is then the only truth. Without
/// this pass the row keeps its last live wording ("running in background")
/// indefinitely, even though the strip and the jobs list say otherwise.
fn settle_background_rows_from_chrome(state: &mut AppState) {
    use crate::state::BackgroundOutcome;
    let settled: Vec<_> = state
        .background_task_labels
        .iter()
        .filter_map(|(task_id, chrome)| match chrome.outcome() {
            BackgroundOutcome::Running => None,
            outcome => Some((
                task_id.clone(),
                outcome,
                chrome.exit_code,
                chrome.duration_ms,
            )),
        })
        .collect();
    for (task_id, outcome, exit_code, duration_ms) in settled {
        match outcome {
            BackgroundOutcome::Running => {}
            BackgroundOutcome::Completed => {
                state.transcript.note_background_settled(
                    &task_id,
                    "exited",
                    exit_code,
                    duration_ms,
                );
            }
            BackgroundOutcome::Failed => {
                state.transcript.note_background_settled(
                    &task_id,
                    "failed",
                    exit_code,
                    duration_ms,
                );
            }
            BackgroundOutcome::Stopped => {
                state.transcript.note_background_settled(
                    &task_id,
                    "stopped",
                    exit_code,
                    duration_ms,
                );
            }
            // No terminal fact exists: the row may say only that its state
            // cannot be proven, never that it finished or failed.
            BackgroundOutcome::Lost | BackgroundOutcome::Conflict | BackgroundOutcome::Unknown => {
                state.transcript.note_background_unknown(&task_id);
            }
        }
    }
}

fn archive_active_plan(state: &mut AppState) {
    if let Some(plan) = state.plan.take() {
        // A fully settled checklist has finished its job as execution chrome.
        // Keep only plans with open or failed work in history; otherwise a
        // completed turn leaves a redundant N/N block above its result.
        if crate::workbench::plan_panel_should_show(&plan) {
            state.transcript.push_plan(plan);
        }
    }
    if state.workbench_focus == crate::state::WorkbenchFocus::Plan {
        state.workbench_focus = crate::state::WorkbenchFocus::Input;
    }
}

fn push_turn_receipt(
    state: &mut AppState,
    status: TurnEndStatus,
    summary: Option<String>,
    detail: Option<String>,
) {
    state.transcript.push_turn_end_with_receipt(
        status,
        state.turn_tool_calls,
        state.elapsed_secs,
        summary,
        detail,
        turn_receipt(state),
    );
}

pub(super) fn start_turn(state: &mut AppState) {
    crate::away_summary::cancel(state);
    state.team.retire_settled(state.elapsed_secs);
    // Finished background tasks stay in the projection as the list page's
    // "recently finished" window; the footer never shows them. A still-running
    // one (a dev server) stays reachable too.
    state.turn_tool_calls = 0;
    // The previous turn's booked usage belongs to its own receipt. A new turn
    // starts with no settlement until the runtime reports one.
    state.settled_turn_usage = None;
    state.status = RuntimeStatus::Busy;
    state.resumable_task = false;
    state.finalization_stage = None;
    state.project_rule_sources.clear();
    // The previous turn's next step is spent — a new turn is under way.
    crate::suggestion::dismiss(state);
    seal_analysis_segment(state);
    // The Active Goal indicator begins with the turn: a new task replaces the
    // last goal with a fresh clock, a continuation keeps its identity and its
    // banked active time.
    let staged = state.staged_goal.take();
    crate::active_goal::begin(state, staged, std::time::Instant::now());
}

/// A segment boundary (assistant start, turn end): freeze any reasoning block
/// still streaming. A clean completion already froze it; one that never got the
/// runtime's completion is marked interrupted rather than presented as done.
fn seal_analysis_segment(state: &mut AppState) {
    state.transcript.finish_thought(None);
}

fn mark_turn_busy(state: &mut AppState) {
    // Real runtime progress: a goal that was waiting to resume is now running.
    if let Some(goal) = state.active_goal.as_mut() {
        goal.note_progress();
    }
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
    restore_history(state, entries, omitted_turns, false);
}

fn restore_history(
    state: &mut AppState,
    entries: Vec<leveler_client_protocol::UiHistoryEntry>,
    omitted_turns: u32,
    keep_in_flight: bool,
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
            model_notice: None,
            thinking: None,
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
            scratch.transcript.finalize_in_flight();
            start_turn(&mut scratch);
        }
        scratch.elapsed_secs = entry.turn_elapsed_ms / 1000;
        match entry.event {
            // Replay offsets are wall-clock history, not this client's live clock.
            RuntimeEvent::ToolTimingUpdated { id, timing } => {
                scratch.transcript.set_tool_timing(&id, Some(timing), None);
            }
            // A durable review is projected directly: the live handler's
            // `away_summary_pending` gate exists to match an asynchronous
            // answer to the request that asked for it, and a replay has no
            // request in flight. It carries the same turn identity the live
            // row did, so it lands after that turn's terminal exactly once.
            RuntimeEvent::AwaySummary { text } => {
                scratch.transcript.push_away_summary(text);
            }
            event => apply_runtime(&mut scratch, event),
        }
    }
    // A turn the log never closed (its runtime died) has no outcome to show.
    if !keep_in_flight {
        scratch.transcript.finalize_in_flight();
    }
    // A history answer is a snapshot taken earlier than the replay. Input the
    // live transcript admitted or settled after that snapshot is newer than
    // it, and only the runtime can settle it; erasing the row because the
    // replay cannot carry it would make an accepted message vanish. Rows the
    // replay produced itself win, so a consumed input never regresses to
    // pending.
    let journal_held: Vec<_> = state
        .transcript
        .inputs()
        .filter(|input| {
            matches!(
                input.state,
                InputDelivery::Pending | InputDelivery::Cancelled(_)
            )
        })
        .map(|input| (input.id.clone(), input.text.clone(), input.state.clone()))
        .collect();
    state.transcript.replace_with(scratch.transcript);
    for (id, text, delivery) in journal_held {
        if !state.transcript.has_input(&id) {
            state.transcript.push_input(id, text, delivery);
        }
    }
    settle_background_rows_from_chrome(state);
    // The rebuilt transcript has its own positions; an index from the previous
    // transcript would name another entry.
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
    state.snapshot_version = Some(session.last_sequence.unwrap_or(0));
    state.session_id = session.id.clone();
    state.repository = session.repository.clone();
    state.branch = session.branch.clone();
    state.goal = session.goal.clone();
    let model_label = session
        .model
        .as_ref()
        .map(|m| m.to_string())
        .unwrap_or_else(|| "Auto".to_string());
    if state.model_label != model_label {
        // The context accounting was measured against the previous model's
        // resolved policy. A different model makes every one of its figures a
        // denominator the next request never uses, so drop it with the token
        // fallback; the refreshed query answers with the new policy's numbers
        // (or nothing, until an assembled request exists).
        state.context.loaded = None;
        state.context_tokens = 0;
        state.token_input = 0;
        state.token_output = 0;
        state.token_cached = 0;
    }
    state.model_label = model_label;
    state.mode = session.mode;
    state.mode_label = mode_label(session.mode).to_string();
    state.available_models = session.available_models.clone();
    state.vision = session.vision;
    // The runtime owns the level; adopt it so a reconnect cannot show a stale
    // local guess. Absent on an old runtime means keep what we have.
    if session.thinking.is_some() {
        state.thinking = session.thinking.clone();
    }
    // Product axes: the session record is the source of truth; adopt when the
    // runtime sends them so a reconnect cannot show a stale local guess. Old
    // runtimes omit the fields — keep the local value then.
    if let Some(collaboration) = &session.collaboration {
        state.collaboration = collaboration.clone();
    }
    // Provenance belongs to the opened session, not to the client: an ordinary
    // native session clears whatever the previous session showed.
    state.foreign_origin = session.foreign_origin.clone();
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
///
/// `settled_after_dispatch` says this client has applied a turn terminal since
/// the command was dispatched. The snapshot was captured as the command was
/// admitted, so every field in it is older than that terminal: adopting its
/// `running` status would reopen a finished turn, and its cleanup would cancel
/// the recap the finished turn just asked for. It has nothing left to say.
pub(super) fn apply_session_confirmation(
    state: &mut AppState,
    session: UiSessionSnapshot,
    settled_after_dispatch: bool,
) {
    if settled_after_dispatch {
        return;
    }
    apply_session_with(state, session, TranscriptAdoption::KeepLive);
}

fn apply_session(state: &mut AppState, session: UiSessionSnapshot) {
    apply_session_with(state, session, TranscriptAdoption::Replace);
}

/// Adopt the runtime's durable projection of the goal this session owes.
///
/// The snapshot is the only authority for WHICH goal is owed, whether a window
/// is executing it, and whether the user's own pause is in force. The live
/// phase stays on [`crate::active_goal::ActiveGoal`], which owns this client's
/// clock; this decides what is owed, not what is running right now.
fn adopt_execution_task(
    state: &mut AppState,
    task: Option<leveler_client_protocol::UiExecutionTask>,
) {
    let Some(task) = task else {
        state.execution_goal = None;
        // Mid-turn the commit's badge is this turn's own fact; the terminal
        // event settles it, never a metadata refresh.
        if !state.is_busy() {
            state.execution_task_active = false;
        }
        return;
    };
    state.execution_task_active = true;
    // An owed goal with no live window is a continuation point: the runtime
    // still accepts a new turn against it. A live window already owns the
    // turn, so there is no finished terminal to resume while it runs.
    state.resumable_task = !task.running;
    // Only a live window executes the goal contract; owed work with no window,
    // or work the user paused, is not running.
    state.goal_mode_active = task.running && state.status == RuntimeStatus::Busy;
    let now = std::time::Instant::now();
    let paused = task.user_paused || !task.running;
    let title =
        crate::active_goal::short_title(&task.objective).unwrap_or_else(|| task.objective.clone());
    match state.active_goal.as_mut() {
        Some(goal) => {
            // A different goal id is not this goal continuing: identity, Phase
            // and the display clock belong to the goal that owns them, so a
            // second goal starts its own window instead of inheriting the
            // first one's pause or elapsed time.
            let switched = state
                .execution_goal
                .as_ref()
                .is_some_and(|previous| previous.goal_id != task.goal_id);
            if switched {
                goal.replace_with_objective(title, task.objective.clone(), now);
            } else {
                goal.adopt_objective(title, task.objective.clone());
            }
            if paused {
                goal.pause(now);
            } else if goal.phase() == crate::active_goal::GoalPhase::Paused {
                // The user released the pause, so the durable row is the newer
                // fact: the display follows it back into the window.
                goal.resume(now);
            }
        }
        None => {
            let mut goal = crate::active_goal::ActiveGoal::running_with_objective(
                title,
                task.objective.clone(),
                now,
            );
            if paused {
                goal.pause(now);
            }
            state.active_goal = Some(goal);
        }
    }
    state.execution_goal = Some(task);
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
    // The runtime's own crash-window evidence, not a client-side guess. An
    // empty list means this session holds no recovery gate right now.
    state.interrupted_tool_calls = session.interrupted_tool_calls.clone();
    // A snapshot is the runtime's own answer. Whatever broke the event stream
    // or left a command's outcome unknown, the client can see canonical state
    // again — the reconnect authority the disconnected state was waiting for.
    state.runtime_connected = true;
    // A snapshot is the recovery authority for the view-integrity errors the
    // transport reported: they were true only while no canonical state could
    // be read. A lost connection has no snapshot and keeps its sticky error.
    let t = state.t();
    if state.notification.as_ref().is_some_and(|notice| {
        notice.level == NotificationLevel::Error
            && (notice.message == t.reconnect_resync_failed
                || notice.message == t.stream_resync_failed)
    }) {
        state.notification = None;
    }
    if switching {
        // Input mode and candidate requests are session-scoped: a recall or
        // scan from the previous session must not land in the new draft.
        state.input_mode = crate::shell_mode::InputMode::Normal;
        state.history_mode = crate::shell_mode::InputMode::Normal;
        state.completion.reset_on_edit();
        // A committed task belongs to the session that committed it.
        state.execution_task_active = false;
    }
    // Snapshot is runtime truth. A pending Shift+Tab from the previous
    // connection must not keep driving the cycle, and a next-step ghost from
    // the transcript this snapshot is about to replace no longer applies.
    state.pending_permission = None;
    crate::suggestion::clear(state);
    crate::suggestion::allow_generated(state);
    crate::away_summary::cancel(state);
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
    // A resync snapshot settles whatever transient status the missing
    // terminal left behind (a daemon restart reports the interrupted turn as
    // no longer running). Without this the footer kept drawing that turn's
    // command heartbeat — `命令运行中` — forever, with no turn owning it. Only
    // Replace rebuilds the view; a confirmation (KeepLive) was captured as a
    // command was admitted and must not erase activity of the live turn.
    if adoption == TranscriptAdoption::Replace && state.status != RuntimeStatus::Busy {
        clear_activity(state);
    }

    // A snapshot that IS the view (opening a session, resyncing after events
    // were missed) replaces the live control queue. Only in-process waiters are
    // included, so stale requests from interrupted turns are never resurrected
    // after a runtime restart.
    //
    // A confirmation snapshot is a point-in-time answer to a command this
    // client sent, captured as the command was admitted. A request the turn
    // raised afterwards is absent from it, so rebuilding the queue here would
    // close a prompt the user never answered — and leave the gated call
    // waiting with no way to decide it. The live event stream stays the truth
    // for the queue; only what the view is built from is the snapshot's job.
    if adoption == TranscriptAdoption::Replace {
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
    }

    // Snapshot `plan` is live-view state. Adopt it only while the runtime says
    // this session is actually running and the plan belongs to the open epoch;
    // idle/terminal history is rebuilt from durable events below, through the
    // same reducer as the live stream.
    state.plan = (state.status == RuntimeStatus::Busy
        && session
            .plan
            .as_ref()
            .is_some_and(|plan| plan.state == UiPlanState::Active))
    .then_some(session.plan)
    .flatten();
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
        state.history_query = None;
        state.agents_list_query = None;
        state.agent_detail_query = None;
        state.child_contribution_query = None;
        state.diff_query = None;
        state.diff_pending = false;
        state.context.track.reset();
        state.context.loaded = None;
        state.usage.track.reset();
        state.usage.loaded = None;
        state.trace.track.reset();
        state.context_tokens = 0;
        state.token_input = 0;
        state.token_output = 0;
        state.token_cached = 0;
        state.settled_turn_usage = None;
        state.project_rule_sources.clear();
        state.background_task_labels.clear();
        seal_analysis_segment(state);
        clear_activity(state);
        state.finalization_stage = None;
        state.turn_tool_calls = 0;
        state.screen_scroll = 0;
        state.pending_attachments.clear();
        state.history_search = None;
        state.command_selected = None;
        // Another session's children are not this session's.
        state.team = crate::multi_agent::TaskTeamView::default();
        // Nor is its open Activity: a detail page must not survive into a
        // session that never had that task — and neither does the page a
        // detail would return to.
        state.activity_open = None;
        state.activity_selected = None;
        state.activity_view = crate::activity::ActivityView::default();
        state.detail_return = None;
        // A side thread is scoped to the run it observed: leaving it behind
        // would let a follow-up reference a conversation this session never
        // had. Return to Main and drop it (the main draft is swapped back in).
        let template = state.image_token_template();
        if state.surface == crate::btw::SurfaceFocus::Btw {
            super::leave_btw(state);
        }
        // Clear the main draft only after it has been swapped back. Neither
        // surface may restore another session's attachment identities.
        state.composer.clear_edit_history();
        state.composer.set_edit_attachments(&[]);
        state.btw = crate::btw::BtwThread::default();
        state.btw.draft.set_image_token_template(&template);
        // The Active Goal belongs to one session's task; another session never
        // continues it.
        state.active_goal = None;
    }
    // A live turn this client did not submit (a reconnect, another client)
    // still owns an Active Goal: adopt the session's authoritative objective so
    // the header reflects the running task.
    if state.status == RuntimeStatus::Busy && state.active_goal.is_none() {
        crate::active_goal::begin(state, None, std::time::Instant::now());
    }
    // A committed task outlives the turn window. The snapshot carries its
    // durable goal so a reconnecting client restores the badge and the real
    // objective — including the paused state of work that is still owed.
    adopt_execution_task(state, session.execution_task.clone());
    // A reconnect snapshot replaces (rather than merges) the active process
    // projection. Its entries come from the process registry, so terminal
    // records retained by the registry or replayed transcript events cannot
    // resurrect here.
    replace_active_background_tasks(
        state,
        &session.active_background_tasks,
        &session.background_tasks,
    );
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
            UiRole::User if leveler_client_protocol::is_compaction_summary_text(&message.text) => {
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
            // Accepted input keeps its journal identity across Resume: the
            // row a client drew live is the row it reopens here.
            UiRole::User
                if message.kind == Some(leveler_client_protocol::UiMessageKind::PendingInput) =>
            {
                state.transcript.push_input(
                    message.id.clone(),
                    message.text.clone(),
                    InputDelivery::Pending,
                );
            }
            UiRole::User
                if message.kind == Some(leveler_client_protocol::UiMessageKind::DeliveredInput) =>
            {
                state.transcript.push_input(
                    message.id.clone(),
                    message.text.clone(),
                    InputDelivery::Delivered,
                );
            }
            UiRole::User
                if message.kind == Some(leveler_client_protocol::UiMessageKind::CancelledInput) =>
            {
                state.transcript.push_input(
                    message.id.clone(),
                    message.text.clone(),
                    InputDelivery::Cancelled(String::new()),
                );
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
    let restored_history = session
        .history
        .as_ref()
        .is_some_and(|history| !history.entries.is_empty());
    if let Some(history) = session
        .history
        .filter(|history| !history.entries.is_empty())
    {
        restore_history(
            state,
            history.entries,
            history.omitted_turns,
            state.is_busy(),
        );
    }
    // User shell executions (history + a still-running one) survive
    // reconnect via the snapshot; blocks are rebuilt in order.
    for shell in &session.user_shells {
        let running = shell.status == "running";
        let background = shell.status == "background";
        // Back-date the start (possibly below zero) so the live runtime
        // keeps counting from the runtime's elapsed, not from zero at
        // reconnect.
        let started = state.motion_clock.as_millis() as i64 - shell.elapsed_secs as i64 * 1000;
        let existed = state.transcript.user_shell_index(&shell.id).is_some();
        state.transcript.push_user_shell_started(
            shell.id.clone(),
            shell.command.clone(),
            shell.cwd.clone(),
            started,
        );
        if existed {
            let clean = crate::shell_render::sanitize_shell_output(&shell.output_tail);
            state
                .transcript
                .replace_user_shell_output(&shell.id, &clean, shell.output_truncated);
        } else {
            if !shell.output_tail.is_empty() {
                let clean = crate::shell_render::sanitize_shell_output(&shell.output_tail);
                state
                    .transcript
                    .append_user_shell_output(&shell.id, &clean, false);
            }
            if shell.output_truncated {
                state.transcript.mark_user_shell_output_truncated(&shell.id);
            }
        }
        state
            .transcript
            .set_user_shell_full_output(&shell.id, shell.full_output.clone());
        if background {
            state
                .transcript
                .background_user_shell(&shell.id, shell.task_id.as_deref().unwrap_or(""));
        } else if !running {
            state.transcript.complete_user_shell(
                &shell.id,
                shell.exit_code,
                shell.elapsed_secs * 1000,
                crate::transcript::UserShellStatus::from_wire(&shell.status),
            );
        }
    }
    if let Some(report) = session.completion_report
        && !restored_history
    {
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
        // The snapshot states the round; restore it verbatim so the reconnected
        // transcript groups the calls exactly as the live stream did. A legacy
        // snapshot carries `None`, and the reducer keeps its old fallback.
        let model_step = tool.model_step;
        // Same for the answer classification: a reconnected client decides the
        // FinalAnswer boundary exactly as the live one did.
        let answer_effect = tool.answer_effect;
        let existing = state
            .transcript
            .tool_calls()
            .into_iter()
            .find(|call| call.id == id)
            .map(|call| call.status);
        if existing.is_some_and(|status| status != crate::transcript::ToolStatus::Running) {
            continue;
        }
        if existing.is_none() {
            state.transcript.push_tool_started(
                tool.id,
                tool.name,
                tool.arguments,
                false,
                started,
                model_step,
                answer_effect,
            );
        }
        state.transcript.resume_tool_clock(&id, started);
        state
            .transcript
            .set_tool_timing(&id, tool.timing, Some(state.elapsed_secs));
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
