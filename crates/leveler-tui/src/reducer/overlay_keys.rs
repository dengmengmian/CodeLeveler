use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use leveler_client_protocol::{
    ClientCommand, CommandId, ModelRef, NotificationLevel, PermissionProfile, UiReviewScope,
    UiThinkingAccess,
};
use leveler_model::ThinkingLevel;

use crate::action::Effect;
use crate::overlay::Overlay;
use crate::overlay::approval::{ApprovalOutcome, ApprovalOverlay};
use crate::overlay::clarification::{ClarificationOutcome, ClarificationOverlay};
use crate::overlay::selection::{SelectionModel, SelectionOption, SelectionOutcome};
use crate::state::{AppState, Notification, PendingInteraction};
use crate::theme::{Theme, ThemeId};

use super::submit::send_message;

/// Route a key to the active overlay, returning any resulting command. Ctrl+C
/// is treated as Esc so it closes the overlay safely .
pub(super) fn handle_overlay_key(state: &mut AppState, key: KeyEvent) -> Vec<Effect> {
    let effective = if key.modifiers.contains(KeyModifiers::CONTROL)
        && matches!(key.code, KeyCode::Char('c'))
    {
        KeyEvent::new(KeyCode::Esc, KeyModifiers::empty())
    } else {
        key
    };

    let Some(overlay) = state.overlay.take() else {
        return Vec::new();
    };
    let effects = match overlay {
        Overlay::Approval(mut ov) => match ov.on_key(effective) {
            ApprovalOutcome::None => {
                state.overlay = Some(Overlay::Approval(ov));
                Vec::new()
            }
            ApprovalOutcome::Decide(decision) => {
                let restore = PendingInteraction::Approval(ov.request);
                let command_id = take_or_new_command_id(state, &restore);
                vec![Effect::SendInteraction {
                    command: ClientCommand::ApprovalDecision {
                        request_id: match &restore {
                            PendingInteraction::Approval(r) => r.id.clone(),
                            _ => unreachable!(),
                        },
                        decision,
                    },
                    restore,
                    command_id,
                }]
            }
        },
        Overlay::ModelPicker(mut sel) => match sel.on_key(effective) {
            SelectionOutcome::None => {
                state.overlay = Some(Overlay::ModelPicker(sel));
                Vec::new()
            }
            SelectionOutcome::Cancel => Vec::new(),
            SelectionOutcome::Confirm(key) => match ModelRef::parse(&key) {
                Some(model) => {
                    state.model_label = model.to_string();
                    // The level is model-specific; the runtime re-projects it for
                    // the new model, so do not keep the previous model's value.
                    state.thinking = None;
                    state.notification = Some(Notification {
                        level: NotificationLevel::Info,
                        // Neutral on purpose: whether the choice also became the
                        // PERSISTED default is the runtime's fact, not this
                        // client's to claim. The runtime says so itself when it
                        // declines (a model only this environment can resolve).
                        message: state
                            .t()
                            .model_switched
                            .replace("{model}", &model.to_string()),
                    });
                    // `/model` switches THIS session. Changing the global
                    // default is a separate, explicit action
                    // (`/model default <ref>`); the other clients already
                    // treat a model choice as session-scoped.
                    vec![Effect::Send(ClientCommand::SelectModel {
                        session_id: state.session_id.clone(),
                        model,
                    })]
                }
                None => Vec::new(),
            },
        },
        Overlay::DefaultModelPicker(mut sel) => match sel.on_key(effective) {
            SelectionOutcome::None => {
                state.overlay = Some(Overlay::DefaultModelPicker(sel));
                Vec::new()
            }
            SelectionOutcome::Cancel => Vec::new(),
            SelectionOutcome::Confirm(key) => match ModelRef::parse(&key) {
                Some(model) => {
                    state.model_label = model.to_string();
                    state.thinking = None;
                    state.notification = Some(Notification {
                        level: NotificationLevel::Info,
                        message: state
                            .t()
                            .model_switched
                            .replace("{model}", &model.to_string()),
                    });
                    // The explicit "use this from now on" command. It also
                    // switches this session, so the header follows at once;
                    // only the runtime may rewrite the persisted default.
                    vec![Effect::Send(ClientCommand::SetDefaultModel {
                        session_id: state.session_id.clone(),
                        model,
                    })]
                }
                None => Vec::new(),
            },
        },
        Overlay::ModePicker(mut sel) => match sel.on_key(effective) {
            SelectionOutcome::None => {
                state.overlay = Some(Overlay::ModePicker(sel));
                Vec::new()
            }
            SelectionOutcome::Cancel => Vec::new(),
            SelectionOutcome::Confirm(key) => {
                let mode = parse_mode(&key);
                state.pending_permission = Some(mode);
                vec![Effect::Send(ClientCommand::SetPermissionProfile {
                    session_id: state.session_id.clone(),
                    mode,
                })]
            }
        },
        Overlay::ThemePicker(mut sel) => match sel.on_key(effective) {
            SelectionOutcome::None => {
                state.overlay = Some(Overlay::ThemePicker(sel));
                Vec::new()
            }
            SelectionOutcome::Cancel => Vec::new(),
            SelectionOutcome::Confirm(key) => {
                apply_theme_id(state, &key);
                Vec::new()
            }
        },
        Overlay::ThinkingPicker(mut sel) => match sel.on_key(effective) {
            SelectionOutcome::None => {
                state.overlay = Some(Overlay::ThinkingPicker(sel));
                Vec::new()
            }
            SelectionOutcome::Cancel => Vec::new(),
            SelectionOutcome::Confirm(key) => apply_thinking_choice(state, &key),
        },
        Overlay::ReviewScopePicker(mut sel) => match sel.on_key(effective) {
            SelectionOutcome::None => {
                state.overlay = Some(Overlay::ReviewScopePicker(sel));
                Vec::new()
            }
            SelectionOutcome::Cancel => {
                // The question is gone, so the words it carried are no longer
                // pending anything.
                state.review_scope_focus = None;
                Vec::new()
            }
            SelectionOutcome::Confirm(key) => apply_review_scope_choice(state, &key),
        },
        Overlay::CollabPicker(mut sel) => match sel.on_key(effective) {
            SelectionOutcome::None => {
                state.overlay = Some(Overlay::CollabPicker(sel));
                Vec::new()
            }
            SelectionOutcome::Cancel => Vec::new(),
            SelectionOutcome::Confirm(key) => super::submit::apply_collab(state, &key),
        },
        Overlay::Clarification(mut ov) => match ov.on_key(effective) {
            ClarificationOutcome::None => {
                state.overlay = Some(Overlay::Clarification(ov));
                Vec::new()
            }
            ClarificationOutcome::Answer(answer) => {
                let restore = PendingInteraction::Clarification(ov.request);
                let command_id = take_or_new_command_id(state, &restore);
                vec![Effect::SendInteraction {
                    command: ClientCommand::AnswerClarification {
                        request_id: match &restore {
                            PendingInteraction::Clarification(r) => r.id.clone(),
                            _ => unreachable!(),
                        },
                        answer,
                        // The answer targets this session's request. A runtime
                        // that restarted has no live waiter to resolve, and
                        // this is what lets it settle the durable request and
                        // continue the goal.
                        session_id: Some(state.session_id.clone()),
                    },
                    restore,
                    command_id,
                }]
            }
        },
        Overlay::CheckpointPicker(mut sel) => match sel.on_key(effective) {
            SelectionOutcome::None => {
                state.overlay = Some(Overlay::CheckpointPicker(sel));
                Vec::new()
            }
            SelectionOutcome::Cancel => Vec::new(),
            SelectionOutcome::Confirm(key) => {
                // Restore rolls back the working tree too, including files
                // created after the checkpoint. Opening the picker is not
                // consent to that; the second overlay names the consequence
                // and starts on Cancel.
                let t = state.t();
                let options = vec![
                    SelectionOption::new(CHECKPOINT_RESTORE_CANCEL, t.approval_deny),
                    SelectionOption::new(key, t.checkpoint_confirm_action),
                ];
                let model = SelectionModel::new(t.checkpoint_confirm_title, options, false)
                    .with_description(t.checkpoint_confirm_body)
                    .without_quick_select();
                state.overlay = Some(Overlay::ConfirmCheckpointRestore(Box::new(model)));
                Vec::new()
            }
        },
        Overlay::ConfirmCheckpointRestore(mut sel) => match sel.on_key(effective) {
            SelectionOutcome::None => {
                state.overlay = Some(Overlay::ConfirmCheckpointRestore(sel));
                Vec::new()
            }
            // Cancelling the confirmation returns to the checkpoint list: the
            // first Esc leaves the confirmation, the second leaves the picker.
            // Nothing was restored.
            SelectionOutcome::Cancel => {
                open_checkpoint_picker(state);
                Vec::new()
            }
            SelectionOutcome::Confirm(key) => {
                if key == CHECKPOINT_RESTORE_CANCEL {
                    open_checkpoint_picker(state);
                    Vec::new()
                } else {
                    vec![Effect::Send(ClientCommand::RestoreCheckpoint {
                        session_id: state.session_id.clone(),
                        checkpoint_id: leveler_client_protocol::CheckpointId::new(key),
                    })]
                }
            }
        },
        Overlay::ConfirmSessionDelete(mut sel) => match sel.on_key(effective) {
            SelectionOutcome::None => {
                state.overlay = Some(Overlay::ConfirmSessionDelete(sel));
                Vec::new()
            }
            SelectionOutcome::Cancel => Vec::new(),
            SelectionOutcome::Confirm(key) => {
                // The cancel row is a normal selection, not a delete: the
                // confirmation only ever deletes the session the user picked
                // by moving onto the delete row.
                if key == SESSION_DELETE_CANCEL {
                    Vec::new()
                } else {
                    let target = leveler_client_protocol::SessionId::new(key);
                    let mut effects = Vec::new();
                    // Deleting the session being viewed would leave the
                    // composer pointed at a row the runtime is about to
                    // remove. Switch to another live session first (or open a
                    // fresh one), then delete the old id.
                    if target == state.session_id {
                        let requester_session_id = state.session_id.clone();
                        if let Some(other) = state
                            .sessions
                            .iter()
                            .find(|session| session.id != target)
                            .map(|session| session.id.clone())
                        {
                            effects.push(Effect::Send(ClientCommand::OpenSessionFor {
                                requester_session_id,
                                session_id: other,
                            }));
                        } else {
                            effects.push(Effect::Send(ClientCommand::NewSessionFor {
                                requester_session_id,
                            }));
                        }
                    }
                    effects.push(Effect::Send(ClientCommand::DeleteSessionFor {
                        requester_session_id: state.session_id.clone(),
                        session_id: target,
                    }));
                    effects
                }
            }
        },
        Overlay::ConfirmSchedule(mut confirm) => match confirm.model.on_key(effective) {
            SelectionOutcome::None => {
                state.overlay = Some(Overlay::ConfirmSchedule(confirm));
                Vec::new()
            }
            // Esc and the cancel row decide nothing: the definition, its grant
            // and the OS registration stay exactly where they were.
            SelectionOutcome::Cancel => Vec::new(),
            SelectionOutcome::Confirm(key) => {
                if key == crate::schedules::CONFIRM_CANCEL {
                    Vec::new()
                } else {
                    crate::schedules::confirmed(state, confirm.purpose.clone())
                }
            }
        },
        Overlay::ConfirmRecovery(mut sel) => match sel.on_key(effective) {
            SelectionOutcome::None => {
                state.overlay = Some(Overlay::ConfirmRecovery(sel));
                Vec::new()
            }
            // Esc and the cancel row mean the same thing: nothing was decided,
            // and the runtime's recovery block stays exactly where it was.
            SelectionOutcome::Cancel => {
                state.notification = Some(Notification {
                    level: NotificationLevel::Info,
                    message: state.t().recovery_cancelled.to_string(),
                });
                Vec::new()
            }
            SelectionOutcome::Confirm(key) if key == RECOVERY_CANCEL => {
                state.notification = Some(Notification {
                    level: NotificationLevel::Info,
                    message: state.t().recovery_cancelled.to_string(),
                });
                Vec::new()
            }
            // The only acknowledgement: mark the calls outcome-unknown and
            // release the gate. It is not a claim that they succeeded, and it
            // never replays them.
            SelectionOutcome::Confirm(_) => {
                vec![Effect::Send(ClientCommand::AcknowledgeCrashWindow {
                    session_id: state.session_id.clone(),
                })]
            }
        },
        Overlay::ConfirmExit(mut ov) => match ov.model.on_key(effective) {
            SelectionOutcome::None => {
                state.overlay = Some(Overlay::ConfirmExit(ov));
                Vec::new()
            }
            // Esc is the safe choice: nothing was stopped, nothing was left
            // running, and the dialog simply closes.
            SelectionOutcome::Cancel => {
                state.disarm_ctrlc();
                state.exit.pending_continue = None;
                Vec::new()
            }
            SelectionOutcome::Confirm(key) => match key.as_str() {
                crate::overlay::exit_confirm::EXIT_OPTION_STOP => vec![Effect::Quit],
                crate::overlay::exit_confirm::EXIT_OPTION_BACKGROUND => {
                    // A disabled option cannot confirm, so reaching here means
                    // the projection said every affected execution can stay
                    // alive. The effect re-checks a fresh snapshot before the
                    // client leaves.
                    state.exit.pending_continue = Some(ov.impact.clone());
                    state.quit_pending = true;
                    state.notification = Some(Notification {
                        level: NotificationLevel::Info,
                        message: state.t().exit_continuing.to_string(),
                    });
                    vec![Effect::ContinueExit {
                        generation: state.exit.generation,
                    }]
                }
                // "Stay" and any future row that is not an action: keep the
                // session, every task, and the draft untouched.
                _ => {
                    state.disarm_ctrlc();
                    state.exit.pending_continue = None;
                    Vec::new()
                }
            },
        },
        Overlay::UnsupportedMedia(mut sel) => match sel.on_key(effective) {
            SelectionOutcome::None => {
                state.overlay = Some(Overlay::UnsupportedMedia(sel));
                Vec::new()
            }
            SelectionOutcome::Cancel => Vec::new(),
            SelectionOutcome::Confirm(key) => match key.as_str() {
                "switch" => {
                    open_model_picker(state);
                    Vec::new()
                }
                // Both "remove images" and "text only" drop the images and
                // send. The sentence names them, so the names go too — leaving
                // `[图片 #1]` in the text would promise the model a picture
                // this message no longer carries.
                _ => {
                    while state.composer.image_token_count() > 0 {
                        state.composer.remove_image_token(1);
                    }
                    state.pending_attachments.clear();
                    send_message(state)
                }
            },
        },
    };
    // Defer promotion while an interaction send is in flight — only promote
    // after the runtime ACK path succeeds (see `perform_effects`).
    let interaction_in_flight = effects
        .iter()
        .any(|e| matches!(e, Effect::SendInteraction { .. }));
    if !interaction_in_flight {
        // Whatever just happened may have closed the active overlay. If so,
        // promote the next parked approval so queued requests are never left
        // unanswered.
        advance_overlay(state);
    }
    effects
}

/// Prefer a sticky id from a previous delivery attempt so retries stay
/// idempotent under command-receipt dedup; otherwise mint a fresh one and
/// remember it until the send is confirmed.
fn take_or_new_command_id(state: &mut AppState, restore: &PendingInteraction) -> CommandId {
    let key = restore.request_key();
    state
        .interaction_command_ids
        .get(&key)
        .cloned()
        .unwrap_or_else(|| {
            let id = CommandId::generate();
            state.interaction_command_ids.insert(key, id.clone());
            id
        })
}

/// Promote the next queued approval to the active overlay, if the overlay slot
/// is now free. A no-op when an overlay is still open (a picker the user opened
/// or an approval already showing), so it never interrupts the current overlay.
pub fn advance_overlay(state: &mut AppState) {
    if state.overlay.is_some() {
        return;
    }
    state.overlay = match state.pending_interactions.pop_front() {
        Some(PendingInteraction::Approval(request)) => {
            Some(Overlay::Approval(Box::new(ApprovalOverlay::new(request))))
        }
        Some(PendingInteraction::Clarification(request)) => Some(Overlay::Clarification(Box::new(
            ClarificationOverlay::new(request),
        ))),
        None => None,
    };
}

pub fn restore_interaction_overlay(state: &mut AppState, restore: PendingInteraction) {
    if let Some(current) = state.overlay.take() {
        match current {
            Overlay::Approval(ov) => state
                .pending_interactions
                .push_front(PendingInteraction::Approval(ov.request)),
            Overlay::Clarification(ov) => state
                .pending_interactions
                .push_front(PendingInteraction::Clarification(ov.request)),
            other => state.overlay = Some(other),
        }
    }
    state.overlay = Some(match restore {
        PendingInteraction::Approval(request) => {
            Overlay::Approval(Box::new(ApprovalOverlay::new(request)))
        }
        PendingInteraction::Clarification(request) => {
            Overlay::Clarification(Box::new(ClarificationOverlay::new(request)))
        }
    });
}

pub(super) fn open_unsupported_media(state: &mut AppState) {
    // Two outcomes, not three: "移除图片后继续" and "仅发送文字" both dropped
    // the images and sent the text, which is one choice wearing two labels —
    // a menu entry that changes nothing is a question the user has to answer
    // for no reason.
    let options = vec![
        SelectionOption::new("switch", state.t().image_switch_model),
        SelectionOption::new("text", state.t().image_text_only),
    ];
    let model = SelectionModel::new(state.t().image_unsupported_title, options, false)
        .with_description(state.t().image_choose_hint);
    state.overlay = Some(Overlay::UnsupportedMedia(Box::new(model)));
}

pub(crate) fn open_model_picker(state: &mut AppState) {
    if state.available_models.is_empty() {
        state.notification = Some(Notification {
            level: NotificationLevel::Warning,
            message: state.t().no_switchable_model.to_string(),
        });
        return;
    }
    let current = state.model_label.clone();
    let options: Vec<SelectionOption> = state
        .available_models
        .iter()
        .map(|m| {
            let key = m.to_string();
            let is_current = key == current;
            SelectionOption::new(key.clone(), key).current(is_current)
        })
        .collect();
    let searchable = options.len() > 6;
    let model =
        SelectionModel::new(state.t().choose_model, options, searchable).focus_key(&current);
    state.overlay = Some(Overlay::ModelPicker(Box::new(model)));
}

/// `/settings` → the global default model. The table is the same runtime
/// catalog `/model` shows; only the command on confirm differs.
pub(crate) fn open_default_model_picker(state: &mut AppState) {
    if state.available_models.is_empty() {
        state.notification = Some(Notification {
            level: NotificationLevel::Warning,
            message: state.t().no_switchable_model.to_string(),
        });
        return;
    }
    let current = state.model_label.clone();
    let options: Vec<SelectionOption> = state
        .available_models
        .iter()
        .map(|m| {
            let key = m.to_string();
            SelectionOption::new(key.clone(), key).current(m.to_string() == current)
        })
        .collect();
    let searchable = options.len() > 6;
    let model = SelectionModel::new(
        state.t().settings_page.row_default_model,
        options,
        searchable,
    )
    .focus_key(&current);
    state.overlay = Some(Overlay::DefaultModelPicker(Box::new(model)));
}

pub(super) fn open_checkpoint_picker(state: &mut AppState) {
    if state.checkpoints.is_empty() {
        state.notification = Some(Notification {
            level: NotificationLevel::Warning,
            message: state.t().checkpoint_none.to_string(),
        });
        return;
    }
    let options: Vec<SelectionOption> = state
        .checkpoints
        .iter()
        .rev()
        .map(|c| SelectionOption::new(c.id.to_string(), format!("#{} {}", c.ordinal, c.label)))
        .collect();
    // The runtime captures a git tree before every turn and restores it here,
    // so this rolls the working tree back too — including deleting files that
    // did not exist at the checkpoint. Saying otherwise would get work lost.
    let model = SelectionModel::new(state.t().checkpoint_picker_title, options, false)
        .with_description(state.t().checkpoint_picker_body);
    state.overlay = Some(Overlay::CheckpointPicker(Box::new(model)));
}

/// The key of the cancel row in the session-delete confirmation. Session ids
/// are UUIDs, so this can never collide with the row it protects.
const SESSION_DELETE_CANCEL: &str = "__cancel__";

/// The cancel row of the checkpoint-restore confirmation; checkpoint ids are
/// generated ids, so this sentinel cannot collide with one.
const CHECKPOINT_RESTORE_CANCEL: &str = "__checkpoint_restore_cancel__";

/// The two rows of the recovery confirmation. Neither is a call id, so they
/// cannot collide with the evidence shown above them.
const RECOVERY_CANCEL: &str = "__recovery_cancel__";
const RECOVERY_CONFIRM: &str = "__recovery_confirm__";

/// Show the crash window the runtime reported and ask for the one explicit
/// decision that releases it: acknowledge that these calls' outcomes are
/// unknown. Each call is evidence, not an action, so its row cannot be
/// confirmed; only cancel and the acknowledgement are selectable.
pub(super) fn open_recovery_confirm(state: &mut AppState) -> Vec<Effect> {
    let t = state.t();
    if state.interrupted_tool_calls.is_empty() {
        state.notification = Some(Notification {
            level: NotificationLevel::Info,
            message: t.recovery_nothing_to_ack.to_string(),
        });
        return Vec::new();
    }
    let mut options = Vec::with_capacity(state.interrupted_tool_calls.len() + 2);
    for call in &state.interrupted_tool_calls {
        let evidence = if call.pending_approval {
            t.recovery_state_pending_approval
        } else {
            match call.state {
                leveler_client_protocol::UiInterruptedCallState::Interrupted => {
                    t.recovery_state_interrupted
                }
                leveler_client_protocol::UiInterruptedCallState::Unknown => {
                    t.recovery_state_unknown
                }
            }
        };
        options.push(
            SelectionOption::new(
                format!("call:{}", call.call_id),
                format!("{} · {}", call.tool, call.call_id),
            )
            .disabled(evidence),
        );
    }
    // Safe by default: the cursor starts on cancel, and a digest cannot pick
    // the acknowledgement.
    options.push(SelectionOption::new(RECOVERY_CANCEL, t.cancel));
    options.push(
        SelectionOption::new(RECOVERY_CONFIRM, t.recovery_confirm).description(t.recovery_warning),
    );
    let model = SelectionModel::new(t.recovery_overlay_title, options, false)
        .with_description(t.recovery_overlay_intro)
        .without_quick_select();
    state.overlay = Some(Overlay::ConfirmRecovery(Box::new(model)));
    Vec::new()
}

/// Ask before deleting a stored session. Deleting is irreversible and the
/// runtime has no undo, so the safe choice (cancel) is the initial cursor and
/// Enter only acts on what the user explicitly moved to.
pub(super) fn open_session_delete_confirm(state: &mut AppState, index: usize) {
    let Some(session) = state.sessions.get(index) else {
        return;
    };
    let id = session.id.as_str().to_string();
    let goal =
        crate::render::truncate_display(session.title.as_deref().unwrap_or(&session.goal), 40);
    let updated = session.updated_at.clone();
    let t = state.t();
    let title = t.session_delete_title;
    let cancel = t.session_delete_cancel;
    let confirm = t.session_delete_confirm;
    let warning = t.session_delete_warning;
    let description = format!("{goal} · {updated} · {warning}");
    let options = vec![
        SelectionOption::new(SESSION_DELETE_CANCEL, cancel),
        SelectionOption::new(id, confirm),
    ];
    let model = SelectionModel::new(title, options, false)
        .with_description(description)
        .without_quick_select();
    state.overlay = Some(Overlay::ConfirmSessionDelete(Box::new(model)));
}

pub(crate) fn open_mode_picker(state: &mut AppState) {
    let current = mode_chip_key(state.mode);
    let t = state.t();
    // Same three PermissionProfile values; labels are the footer chips.
    let options = vec![
        SelectionOption::new("ask", "ask")
            .description(t.perm_ask)
            .current(current == "ask"),
        SelectionOption::new("auto", "auto")
            .description(t.perm_auto)
            .current(current == "auto"),
        SelectionOption::new("full", "full")
            .description(t.perm_full_short)
            .current(current == "full"),
    ];
    let model = SelectionModel::new(t.overlay_mode, options, false).focus_key(current);
    state.overlay = Some(Overlay::ModePicker(Box::new(model)));
}

pub(super) fn open_collab_picker(state: &mut AppState) {
    let current = state.collaboration.as_str();
    let t = state.t();
    let options = vec![
        SelectionOption::new("chat", "chat")
            .description(t.collab_chat)
            .current(current == "chat"),
        SelectionOption::new("plan", "plan")
            .description(t.collab_plan)
            .current(current == "plan"),
        SelectionOption::new("goal", "goal")
            .description(t.collab_goal)
            .current(current == "goal"),
    ];
    let model = SelectionModel::new(t.overlay_collab, options, false).focus_key(current);
    state.overlay = Some(Overlay::CollabPicker(Box::new(model)));
}

/// `/review` with no argument: choose what to review. The default is the
/// current task, which is the narrowest honest object (the latest accepted
/// delivery's own files, never every file another session touched).
pub(super) fn open_review_scope_picker(state: &mut AppState) {
    let t = state.t();
    let review = &t.review;
    let options = vec![
        SelectionOption::new("current_task", review.scope_current_task)
            .description(review.scope_current_task_desc)
            .recommended(true)
            .current(true),
        SelectionOption::new("workspace", review.scope_workspace)
            .description(review.scope_workspace_desc),
        SelectionOption::new("project", review.scope_project)
            .description(review.scope_project_desc),
        SelectionOption::new("session", review.scope_session)
            .description(review.scope_session_desc),
        SelectionOption::new("commit", review.scope_commit).description(review.scope_commit_desc),
        SelectionOption::new("branch", review.scope_branch).description(review.scope_branch_desc),
        SelectionOption::new("paths", review.scope_paths).description(review.scope_paths_desc),
    ];
    let model =
        SelectionModel::new(review.scope_picker_title, options, false).focus_key("current_task");
    state.overlay = Some(Overlay::ReviewScopePicker(Box::new(model)));
}

/// Act on the scope picker. Scopes that name a target only PREFILL the
/// composer: launching a commit review with an empty rev would be a lie about
/// what was reviewed.
fn apply_review_scope_choice(state: &mut AppState, key: &str) -> Vec<Effect> {
    let prefix = match key {
        "current_task" => return start_review(state, UiReviewScope::CurrentTask),
        "workspace" => return start_review(state, UiReviewScope::Workspace),
        "project" => return start_review(state, UiReviewScope::Project),
        "session" => return start_review(state, UiReviewScope::Session { session_id: None }),
        "commit" => "/review @commit ",
        "branch" => "/review @branch ",
        "paths" => "/review @path ",
        _ => return Vec::new(),
    };
    let message = state.t().review.target_prompt.to_string();
    state.composer.replace(prefix);
    state.notification = Some(Notification {
        level: NotificationLevel::Info,
        message,
    });
    Vec::new()
}

fn start_review(state: &mut AppState, scope: UiReviewScope) -> Vec<Effect> {
    // A focus typed before the runtime asked for a scope is part of the same
    // request: choosing the OBJECT must not drop it.
    let focus = state.review_scope_focus.take().unwrap_or_default();
    vec![Effect::Send(ClientCommand::RunReview {
        session_id: state.session_id.clone(),
        focus,
        scope,
        model: None,
    })]
}

pub(crate) fn open_theme_picker(state: &mut AppState) {
    let current = state.theme.id.as_str();
    let t = state.t();
    let options = vec![
        SelectionOption::new(ThemeId::Auto.as_str(), t.theme_auto_label)
            .description(t.theme_auto_desc)
            .current(current == ThemeId::Auto.as_str()),
        SelectionOption::new(ThemeId::Dark.as_str(), t.theme_dark_label)
            .description(t.theme_dark_desc)
            .current(current == ThemeId::Dark.as_str()),
        SelectionOption::new(ThemeId::Light.as_str(), t.theme_light_label)
            .description(t.theme_light_desc)
            .current(current == ThemeId::Light.as_str()),
        SelectionOption::new(ThemeId::HighContrast.as_str(), t.theme_hc_label)
            .description(t.theme_hc_desc)
            .current(current == ThemeId::HighContrast.as_str()),
    ];
    let model = SelectionModel::new(t.overlay_theme, options, false).focus_key(current);
    state.overlay = Some(Overlay::ThemePicker(Box::new(model)));
}

/// The choice that clears a session override, as opposed to choosing `auto`.
///
/// It is not a Thinking Level: it means "go back to the configured default",
/// which is why the picker offers it separately and only when there is an
/// override to clear.
pub(super) const THINKING_RESET_KEY: &str = "__reset__";

/// `/thinking` with no argument: pick a level for this session.
///
/// The list is the runtime's capability-driven set — the levels this model can
/// actually distinguish — so a model with two levels offers three things and
/// not seven, and a provider's own parameter never appears.
pub(crate) fn open_thinking_picker(state: &mut AppState) {
    let t = state.t();
    let Some(thinking) = state.thinking.clone() else {
        state.notification = Some(Notification {
            level: NotificationLevel::Warning,
            message: t.thinking_unknown.to_string(),
        });
        return;
    };
    match thinking.access {
        UiThinkingAccess::Unsupported => {
            state.notification = Some(Notification {
                level: NotificationLevel::Info,
                message: t.thinking_unsupported.to_string(),
            });
            return;
        }
        UiThinkingAccess::Fixed => {
            state.notification = Some(Notification {
                level: NotificationLevel::Info,
                message: t.thinking_fixed.to_string(),
            });
            return;
        }
        UiThinkingAccess::Adjustable => {}
    }
    let describe = |level: ThinkingLevel| match level {
        ThinkingLevel::Auto => t.thinking_lv_auto,
        ThinkingLevel::Off => t.thinking_lv_off,
        ThinkingLevel::Minimal => t.thinking_lv_minimal,
        ThinkingLevel::Low => t.thinking_lv_low,
        ThinkingLevel::Medium => t.thinking_lv_medium,
        ThinkingLevel::High => t.thinking_lv_high,
        ThinkingLevel::Max => t.thinking_lv_max,
    };
    let mut options: Vec<SelectionOption> = Vec::new();
    // Only when there is something to clear: a user must not have to remember a
    // command to get back to the configured level.
    if thinking.session_override.is_some() {
        options.push(
            SelectionOption::new(
                THINKING_RESET_KEY,
                format!(
                    "{} ({})",
                    t.thinking_reset,
                    t.thinking_default
                        .replacen("{}", thinking.configured.as_str(), 1)
                ),
            )
            .description(t.thinking_description),
        );
    }
    for level in &thinking.choices {
        // The word is what the user types and what the status line shows, so it
        // is on the row; the sentence beside it says what the word means.
        options.push(
            SelectionOption::new(level.as_str(), format!("{level}  {}", describe(*level)))
                .current(*level == thinking.current),
        );
    }
    let title = format!(
        "{}  {}  {}",
        t.thinking_title,
        t.thinking_current
            .replacen("{}", thinking.current.as_str(), 1),
        t.thinking_default
            .replacen("{}", thinking.configured.as_str(), 1),
    );
    let description = if thinking.effective != thinking.current {
        t.thinking_configured_unavailable
            .replacen("{}", thinking.current.as_str(), 1)
            .replacen(
                "{}",
                thinking
                    .choices
                    .iter()
                    .map(|level| level.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
                    .as_str(),
                1,
            )
    } else {
        t.thinking_description.to_string()
    };
    let model = SelectionModel::new(title, options, false)
        .with_description(description)
        .focus_key(thinking.current.as_str());
    state.overlay = Some(Overlay::ThinkingPicker(Box::new(model)));
}

/// Apply one Thinking Level choice to this session.
fn apply_thinking_choice(state: &mut AppState, key: &str) -> Vec<Effect> {
    let level = if key == THINKING_RESET_KEY {
        None
    } else {
        match ThinkingLevel::parse(key) {
            Some(level) => Some(level),
            None => return Vec::new(),
        }
    };
    let t = state.t();
    state.notification = Some(Notification {
        level: NotificationLevel::Info,
        message: t.thinking_requested.replacen(
            "{}",
            &level.map_or("reset".to_string(), |l| l.to_string()),
            1,
        ),
    });
    vec![Effect::Send(ClientCommand::SetThinkingLevel {
        session_id: state.session_id.clone(),
        level,
    })]
}

/// Apply a named theme (picker confirm or `/theme <id>`). Invalid ids leave state unchanged
/// and set a warning notification listing valid values.
pub(super) fn apply_theme_id(state: &mut AppState, raw: &str) {
    let no_color = Theme::env_no_color();
    let Some(next) = ThemeId::parse(raw) else {
        state.notification = Some(Notification {
            level: NotificationLevel::Warning,
            message: state.t().theme_unknown.replace("{raw}", raw).replace(
                "{choices}",
                &ThemeId::ALL
                    .iter()
                    .map(|id| id.as_str())
                    .collect::<Vec<_>>()
                    .join(", "),
            ),
        });
        return;
    };
    state.theme = Theme::resolve(next, no_color);
    state.dark = state.theme.is_dark();
    if let Some((r, g, b)) = crate::theme::sample_rgb(state.theme.surface.canvas)
        && let Some(image) = state.welcome_image.as_mut()
    {
        image.set_background([r, g, b, 255]);
    }
    if let Err(err) = crate::theme_config::save_theme_id(next) {
        tracing::warn!(%err, "failed to persist theme id");
    }
    let mono = if no_color { " (NO_COLOR)" } else { "" };
    state.notification = Some(Notification {
        level: NotificationLevel::Info,
        message: state
            .t()
            .theme_switched
            .replace("{theme}", next.as_str())
            .replace("{mono}", mono),
    });
}

fn parse_mode(key: &str) -> PermissionProfile {
    match key {
        "ask" => PermissionProfile::RequestApproval,
        "auto" => PermissionProfile::Assisted,
        "full" => PermissionProfile::FullAccess,
        other => PermissionProfile::parse(other).unwrap_or(PermissionProfile::Assisted),
    }
}

fn mode_chip_key(mode: PermissionProfile) -> &'static str {
    match mode {
        PermissionProfile::RequestApproval => "ask",
        PermissionProfile::Assisted => "auto",
        PermissionProfile::FullAccess => "full",
    }
}
