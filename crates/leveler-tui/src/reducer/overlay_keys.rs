use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use leveler_client_protocol::{
    ClientCommand, CommandId, ModelRef, NotificationLevel, PermissionProfile,
};

use crate::action::Effect;
use crate::overlay::Overlay;
use crate::overlay::approval::{ApprovalOutcome, ApprovalOverlay};
use crate::overlay::clarification::{ClarificationOutcome, ClarificationOverlay};
use crate::overlay::selection::{SelectionModel, SelectionOption, SelectionOutcome};
use crate::state::{AppState, Notification, PendingInteraction};
use crate::theme::{Theme, ThemeId};

use super::runtime_apply::mode_label;
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
                    state.notification = Some(Notification {
                        level: NotificationLevel::Info,
                        message: format!("已切换模型并设为默认: {model}"),
                    });
                    vec![Effect::Send(ClientCommand::SelectModel {
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
                state.mode = mode;
                state.mode_label = mode_label(mode).to_string();
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
                vec![Effect::Send(ClientCommand::RestoreCheckpoint {
                    session_id: state.session_id.clone(),
                    checkpoint_id: leveler_client_protocol::CheckpointId::new(key),
                })]
            }
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
                // Both "remove images" and "text only" drop the images and send.
                _ => {
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
    let options = vec![
        SelectionOption::new("switch", "切换到支持图片的模型"),
        SelectionOption::new("remove", "移除图片后继续"),
        SelectionOption::new("text", "仅发送文字"),
    ];
    let model = SelectionModel::new("当前模型不支持图片", options, false)
        .with_description("请选择如何继续");
    state.overlay = Some(Overlay::UnsupportedMedia(Box::new(model)));
}

pub(super) fn open_model_picker(state: &mut AppState) {
    if state.available_models.is_empty() {
        state.notification = Some(Notification {
            level: NotificationLevel::Warning,
            message: "无可切换的模型".to_string(),
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
    let model = SelectionModel::new("选择模型", options, searchable).focus_key(&current);
    state.overlay = Some(Overlay::ModelPicker(Box::new(model)));
}

pub(super) fn open_checkpoint_picker(state: &mut AppState) {
    if state.checkpoints.is_empty() {
        state.notification = Some(Notification {
            level: NotificationLevel::Warning,
            message: "暂无检查点".to_string(),
        });
        return;
    }
    let options: Vec<SelectionOption> = state
        .checkpoints
        .iter()
        .rev()
        .map(|c| SelectionOption::new(c.id.to_string(), format!("#{} {}", c.ordinal, c.label)))
        .collect();
    let model = SelectionModel::new("恢复到检查点", options, false)
        .with_description("将对话回退到此处（不回退文件，请用 git）");
    state.overlay = Some(Overlay::CheckpointPicker(Box::new(model)));
}

pub(super) fn open_mode_picker(state: &mut AppState) {
    let current = mode_key(state.mode);
    let t = state.t();
    // Permission tiers (not collaboration-plan steps).
    let options = vec![
        SelectionOption::new("request_approval", t.perm_readonly)
            .description(t.mode_plan_desc)
            .current(current == "request_approval"),
        SelectionOption::new("assisted", t.perm_workspace)
            .description(t.mode_write_desc)
            .current(current == "assisted"),
        SelectionOption::new("full_access", t.perm_full)
            .description(t.mode_full_desc)
            .current(current == "full_access"),
    ];
    let model = SelectionModel::new(t.overlay_mode, options, false).focus_key(current);
    state.overlay = Some(Overlay::ModePicker(Box::new(model)));
}

pub(super) fn open_theme_picker(state: &mut AppState) {
    let current = state.theme.id.as_str();
    let t = state.t();
    let options = vec![
        SelectionOption::new(ThemeId::Ion.as_str(), t.theme_ion_label)
            .description(t.theme_ion_desc)
            .current(current == ThemeId::Ion.as_str()),
        SelectionOption::new(ThemeId::Night.as_str(), t.theme_night_label)
            .description(t.theme_night_desc)
            .current(current == ThemeId::Night.as_str()),
        SelectionOption::new(ThemeId::Day.as_str(), t.theme_day_label)
            .description(t.theme_day_desc)
            .current(current == ThemeId::Day.as_str()),
    ];
    let model = SelectionModel::new(t.overlay_theme, options, false).focus_key(current);
    state.overlay = Some(Overlay::ThemePicker(Box::new(model)));
}

/// Apply a named theme (picker confirm or `/theme <id>`). Invalid ids leave state unchanged
/// and set a warning notification listing valid values.
pub(super) fn apply_theme_id(state: &mut AppState, raw: &str) {
    let no_color = Theme::env_no_color();
    let Some(next) = ThemeId::parse(raw) else {
        state.notification = Some(Notification {
            level: NotificationLevel::Warning,
            message: format!(
                "未知主题 `{raw}`；可选: {}",
                ThemeId::ALL
                    .iter()
                    .map(|id| id.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
        });
        return;
    };
    state.theme = Theme::resolve(next, no_color);
    state.dark = !matches!(next, ThemeId::Day) && !no_color;
    if let Err(err) = crate::theme_config::save_theme_id(next) {
        tracing::warn!(%err, "failed to persist theme id");
    }
    let mono = if no_color { " (NO_COLOR)" } else { "" };
    state.notification = Some(Notification {
        level: NotificationLevel::Info,
        message: format!("主题: {}{mono}", next.as_str()),
    });
}

fn parse_mode(key: &str) -> PermissionProfile {
    PermissionProfile::parse(key).unwrap_or(PermissionProfile::Assisted)
}

fn mode_key(mode: PermissionProfile) -> &'static str {
    mode.as_str()
}
