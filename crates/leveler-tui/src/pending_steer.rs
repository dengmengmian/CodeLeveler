//! Controls for the runtime's pending input projection.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use leveler_client_protocol::{ClientCommand, MessageId, SteerAction};
use ratatui::{
    Frame,
    layout::Rect,
    style::Style,
    text::{Line, Span},
    widgets::Paragraph,
};
use unicode_width::UnicodeWidthStr;

use crate::{
    action::Effect,
    composer::Composer,
    screen::Screen,
    shell_mode::InputMode,
    state::{AppState, WorkbenchFocus},
    transcript::{InputDelivery, UserInputBlock},
};

#[derive(Debug, Clone)]
enum Control {
    Details,
    SendAll,
    Send(MessageId),
    Edit(MessageId),
    Withdraw(MessageId),
}

#[derive(Default)]
pub(crate) struct SteerUi {
    pub(crate) edit: Option<Edit>,
    starting: Option<String>,
    selected: usize,
    follow_selection: bool,
    hits: Vec<(Rect, Control)>,
}

impl std::fmt::Debug for SteerUi {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("SteerUi")
            .field("editing", &self.edit.is_some())
            .field("selected", &self.selected)
            .finish_non_exhaustive()
    }
}

pub(crate) struct Edit {
    id: MessageId,
    lease_id: String,
    revision: i64,
    deadline: i64,
    saving: bool,
    draft: Composer,
    mode: InputMode,
    history_mode: InputMode,
}

fn waiting(state: &AppState) -> impl Iterator<Item = &UserInputBlock> {
    state
        .transcript
        .journal_inputs()
        .iter()
        .filter(|row| row.state == InputDelivery::Pending)
}

pub(crate) fn rows(state: &AppState) -> u16 {
    if state.steer.edit.is_some() {
        return 1;
    }
    if state.transcript.journal_inputs().iter().any(|row| {
        matches!(
            row.state,
            InputDelivery::Pending | InputDelivery::Sending | InputDelivery::Unknown
        )
    }) {
        2
    } else {
        0
    }
}

fn editing(row: &UserInputBlock) -> bool {
    row.edit_expires_ms
        .is_some_and(|deadline| deadline > leveler_core::now().timestamp_millis())
}

fn send(state: &AppState, action: SteerAction) -> Vec<Effect> {
    vec![Effect::Send(ClientCommand::ManageSteer {
        session_id: state.session_id.clone(),
        action,
    })]
}

fn open(state: &mut AppState) -> Vec<Effect> {
    if waiting(state).next().is_none() || state.steer.edit.is_some() {
        return Vec::new();
    }
    state.active_screen = Screen::PendingSteer;
    state.screen_scroll = 0;
    state.steer.selected = 0;
    send(state, SteerAction::List)
}

fn activate(state: &mut AppState, control: Control) -> Vec<Effect> {
    let id = match &control {
        Control::Details => return open(state),
        Control::SendAll if state.runtime_connected && state.is_busy() => {
            return send(state, SteerAction::PrioritizeAll);
        }
        Control::SendAll => return Vec::new(),
        Control::Send(id) | Control::Edit(id) | Control::Withdraw(id) => id,
    };
    if !state.runtime_connected
        || !state.is_busy()
        || state.steer.starting.is_some()
        || state.steer.edit.is_some()
    {
        return Vec::new();
    }
    let Some(row) = waiting(state).find(|row| &row.id == id) else {
        return Vec::new();
    };
    let message_id = row.id.clone();
    let revision = row.revision;
    let action = match control {
        Control::Send(_) if !row.priority && !editing(row) => SteerAction::Prioritize {
            message_id,
            revision,
        },
        Control::Edit(_) if !editing(row) => {
            let lease_id = leveler_core::new_uuid_string();
            state.steer.starting = Some(lease_id.clone());
            SteerAction::BeginEdit {
                message_id,
                revision,
                lease_id,
            }
        }
        Control::Withdraw(_) if !editing(row) => SteerAction::Withdraw {
            message_id,
            revision,
        },
        _ => return Vec::new(),
    };
    send(state, action)
}

pub(crate) fn completed(state: &mut AppState, command: &ClientCommand, accepted: bool) {
    let ClientCommand::ManageSteer { session_id, action } = command else {
        return;
    };
    if session_id != &state.session_id {
        return;
    }
    match action {
        SteerAction::BeginEdit {
            message_id,
            lease_id,
            revision,
        } if state.steer.starting.as_ref() == Some(lease_id) => {
            state.steer.starting = None;
            if !accepted || !state.runtime_connected {
                return;
            }
            let Some(row) = waiting(state).find(|row| &row.id == message_id) else {
                return;
            };
            let text = row.text.clone();
            let deadline = row
                .edit_expires_ms
                .unwrap_or_else(|| leveler_core::now().timestamp_millis() + 90_000);
            let draft = std::mem::take(&mut state.composer);
            let mode = std::mem::take(&mut state.input_mode);
            let history_mode = std::mem::take(&mut state.history_mode);
            state.composer.replace(text);
            state.steer.edit = Some(Edit {
                id: message_id.clone(),
                lease_id: lease_id.clone(),
                revision: revision + 1,
                deadline,
                draft,
                mode,
                history_mode,
                saving: false,
            });
            state.active_screen = Screen::Conversation;
            state.workbench_focus = WorkbenchFocus::Input;
            state.completion.reset_on_edit();
        }
        SteerAction::SaveEdit { lease_id, .. } | SteerAction::CancelEdit { lease_id, .. }
            if state
                .steer
                .edit
                .as_ref()
                .is_some_and(|edit| &edit.lease_id == lease_id) =>
        {
            if accepted {
                restore_draft(state);
            } else if let Some(edit) = state.steer.edit.as_mut() {
                edit.saving = false;
            }
        }
        _ => {}
    }
}

pub(crate) fn restore_draft(state: &mut AppState) {
    state.steer.starting = None;
    if let Some(edit) = state.steer.edit.take() {
        state.composer = edit.draft;
        state.input_mode = edit.mode;
        state.history_mode = edit.history_mode;
        state.completion.reset_on_edit();
    }
}

pub(crate) fn reconcile(state: &mut AppState) {
    let expired = state.steer.edit.as_ref().is_some_and(|edit| {
        !state.runtime_connected
            || leveler_core::now().timestamp_millis() >= edit.deadline
            || !waiting(state).any(|row| row.id == edit.id && row.revision <= edit.revision)
    });
    if expired {
        restore_draft(state);
    }
}

pub(crate) fn key(state: &mut AppState, key: KeyEvent) -> Option<Vec<Effect>> {
    if let Some(edit) = state.steer.edit.as_mut()
        && key.modifiers.is_empty()
        && matches!(key.code, KeyCode::Esc | KeyCode::Enter | KeyCode::Tab)
    {
        if edit.saving || key.code == KeyCode::Tab {
            return Some(Vec::new());
        }
        let id = edit.id.clone();
        let lease_id = edit.lease_id.clone();
        let revision = edit.revision;
        let action = if key.code == KeyCode::Enter {
            let content = state.composer.text().trim().to_string();
            if content.is_empty() {
                return Some(Vec::new());
            }
            SteerAction::SaveEdit {
                message_id: id,
                revision,
                lease_id,
                content,
            }
        } else {
            SteerAction::CancelEdit {
                message_id: id,
                revision,
                lease_id,
            }
        };
        edit.saving = true;
        return Some(send(state, action));
    }
    if state.active_screen == Screen::Conversation
        && state.history_search.is_none()
        && key.modifiers == KeyModifiers::ALT
        && key.code == KeyCode::Char('s')
        && waiting(state).next().is_some()
    {
        return Some(open(state));
    }
    if state.active_screen != Screen::PendingSteer {
        return None;
    }
    let count = waiting(state).count();
    match key.code {
        KeyCode::Esc => state.active_screen = Screen::Conversation,
        KeyCode::Up | KeyCode::Down => {
            let selected = state.steer.selected.min(count.saturating_sub(1));
            state.steer.selected = if key.code == KeyCode::Up {
                selected.saturating_sub(1)
            } else {
                (selected + 1).min(count.saturating_sub(1))
            };
            state.steer.follow_selection = true;
        }
        KeyCode::PageUp => state.screen_scroll = state.screen_scroll.saturating_sub(6),
        KeyCode::PageDown => state.screen_scroll = state.screen_scroll.saturating_add(6),
        KeyCode::Char('a') => return Some(activate(state, Control::SendAll)),
        KeyCode::Enter | KeyCode::Char('e') | KeyCode::Delete => {
            let id = waiting(state)
                .nth(state.steer.selected.min(count.saturating_sub(1)))
                .map(|row| row.id.clone());
            if let Some(id) = id {
                let control = match key.code {
                    KeyCode::Enter => Control::Send(id),
                    KeyCode::Delete => Control::Withdraw(id),
                    _ => Control::Edit(id),
                };
                return Some(activate(state, control));
            }
        }
        _ => {}
    }
    Some(Vec::new())
}

pub(crate) fn mouse(state: &mut AppState, mouse: MouseEvent) -> Option<Vec<Effect>> {
    if mouse.kind == MouseEventKind::Down(MouseButton::Left)
        && let Some((_, action)) = state.steer.hits.iter().find(|(area, _)| {
            (area.y..area.bottom()).contains(&mouse.row)
                && (area.x..area.right()).contains(&mouse.column)
        })
    {
        return Some(activate(state, action.clone()));
    }
    if state.active_screen != Screen::PendingSteer {
        return None;
    }
    match mouse.kind {
        MouseEventKind::ScrollUp => state.screen_scroll = state.screen_scroll.saturating_sub(3),
        MouseEventKind::ScrollDown => state.screen_scroll = state.screen_scroll.saturating_add(3),
        MouseEventKind::Down(MouseButton::Left)
            if state
                .back_hit
                .is_some_and(|(y, x0, x1)| y == mouse.row && (x0..x1).contains(&mouse.column)) =>
        {
            state.active_screen = Screen::Conversation
        }
        _ => {}
    }
    Some(Vec::new())
}

pub(crate) fn clear_hits(state: &mut AppState) {
    state.steer.hits.clear();
}

type ControlLine = (Line<'static>, Vec<(u16, u16, Control)>);

fn control_line(
    state: &AppState,
    label: String,
    controls: Vec<(&'static str, Control)>,
    width: u16,
) -> ControlLine {
    let width = usize::from(width);
    let muted = Style::default().fg(state.theme.text.muted);
    let mut cells = UnicodeWidthStr::width(label.as_str());
    let mut line = Line::from(Span::styled(
        crate::render::truncate_display(&label, width),
        muted,
    ));
    let mut hits = Vec::new();
    for (caption, action) in controls {
        let caption_width = UnicodeWidthStr::width(caption);
        if cells + 3 + caption_width > width {
            break;
        }
        line.spans.push(Span::styled(" · ", muted));
        cells += 3;
        let style = if matches!(
            action,
            Control::Send(_) | Control::SendAll | Control::Details
        ) {
            Style::default().fg(state.theme.status.info)
        } else {
            Style::default().fg(state.theme.text.secondary)
        };
        line.spans.push(Span::styled(caption, style));
        hits.push((cells as u16, caption_width as u16, action));
        cells += caption_width;
    }
    (line, hits)
}

fn status(state: &AppState, row: &UserInputBlock) -> &'static str {
    if editing(row) {
        state.t().steer_reserved
    } else if row.priority {
        state.t().steer_safety
    } else {
        state.t().input_state_pending
    }
}

fn row_controls(state: &AppState, row: &UserInputBlock) -> Vec<(&'static str, Control)> {
    if !state.runtime_connected || !state.is_busy() || editing(row) {
        return Vec::new();
    }
    let t = state.t();
    let mut actions = Vec::new();
    if !row.priority {
        actions.push((t.steer_send, Control::Send(row.id.clone())));
    }
    actions.push((t.pending_input_edit, Control::Edit(row.id.clone())));
    actions.push((t.pending_input_delete, Control::Withdraw(row.id.clone())));
    actions
}

fn paint(
    frame: &mut Frame,
    area: Rect,
    lines: Vec<ControlLine>,
    scroll: usize,
    state: &mut AppState,
) {
    let mut visible = Vec::new();
    for (y, (line, hits)) in lines
        .into_iter()
        .skip(scroll)
        .take(area.height.into())
        .enumerate()
    {
        for (x, width, action) in hits {
            state
                .steer
                .hits
                .push((Rect::new(area.x + x, area.y + y as u16, width, 1), action));
        }
        visible.push(line);
    }
    frame.render_widget(Paragraph::new(visible), area);
}

pub(crate) fn render_compact(frame: &mut Frame, area: Rect, state: &mut AppState) {
    if area.is_empty() {
        return;
    }
    let t = state.t();
    let muted = Style::default().fg(state.theme.text.muted);
    let secondary = Style::default().fg(state.theme.text.secondary);
    if state.steer.edit.is_some() {
        frame.render_widget(Paragraph::new(Line::styled(t.steer_editing, muted)), area);
        return;
    }
    let pending = waiting(state).collect::<Vec<_>>();
    let Some(row) = pending.first().copied().or_else(|| {
        state
            .transcript
            .journal_inputs()
            .iter()
            .find(|row| matches!(row.state, InputDelivery::Sending | InputDelivery::Unknown))
    }) else {
        return;
    };
    let preview = crate::render::truncate_display(&format!("  {}", row.text), area.width.into());
    let preview_width = UnicodeWidthStr::width(preview.as_str()) as u16;
    let preview_hit = if row.state == InputDelivery::Pending && preview_width > 0 {
        vec![(0, preview_width.min(area.width), Control::Details)]
    } else {
        Vec::new()
    };
    let controls = match row.state {
        InputDelivery::Pending if pending.len() > 1 => {
            let mut controls = Vec::new();
            if state.runtime_connected && state.is_busy() && pending.iter().any(|row| !row.priority)
            {
                controls.push((t.steer_send_all, Control::SendAll));
            }
            controls.push((t.steer_details, Control::Details));
            control_line(
                state,
                format!(
                    "  ↳ {}",
                    t.pending_steer_count
                        .replacen("{}", &pending.len().to_string(), 1)
                ),
                controls,
                area.width,
            )
        }
        InputDelivery::Pending => control_line(
            state,
            format!("  ↳ {}", status(state, row)),
            row_controls(state, row),
            area.width,
        ),
        InputDelivery::Sending => control_line(
            state,
            format!("  ↳ {}", t.input_state_sending),
            Vec::new(),
            area.width,
        ),
        _ => control_line(
            state,
            format!("  ↳ {}", t.input_state_unknown),
            Vec::new(),
            area.width,
        ),
    };
    paint(
        frame,
        area,
        vec![(Line::styled(preview, secondary), preview_hit), controls],
        0,
        state,
    );
}

pub(crate) fn render_detail(frame: &mut Frame, area: Rect, state: &mut AppState) {
    let t = state.t();
    let layout = crate::secondary::layout(area, 0);
    let page = crate::secondary::SecondaryPage {
        title: t.steer_title,
        status: crate::secondary::main_status_spans(state, area.width.into()),
        hint: t.steer_hint,
    };
    crate::secondary::draw_header(frame, state, &layout, &page);
    crate::secondary::draw_footer(frame, &layout, &page, &state.theme);
    if layout.content.is_empty() {
        return;
    }
    let width = layout.content.width;
    let selected = state
        .steer
        .selected
        .min(waiting(state).count().saturating_sub(1));
    let mut selected_line = 0;
    let mut lines = Vec::new();
    for (index, row) in waiting(state).enumerate() {
        let prefix = if index == selected { "› " } else { "  " };
        for (line_index, text) in
            crate::render::wrap(&row.text, usize::from(width.saturating_sub(2).max(1)))
                .into_iter()
                .enumerate()
        {
            let marker = if line_index == 0 { prefix } else { "  " };
            lines.push((
                Line::styled(
                    format!("{marker}{text}"),
                    Style::default().fg(state.theme.text.primary),
                ),
                Vec::new(),
            ));
        }
        if index == selected {
            selected_line = lines.len();
        }
        lines.push(control_line(
            state,
            format!("  ↳ {}", status(state, row)),
            row_controls(state, row),
            width,
        ));
        lines.push((Line::default(), Vec::new()));
    }
    if lines.is_empty() {
        lines.push((
            Line::styled(t.steer_empty, Style::default().fg(state.theme.text.muted)),
            Vec::new(),
        ));
    } else if state.runtime_connected && state.is_busy() && waiting(state).any(|row| !row.priority)
    {
        lines.push(control_line(
            state,
            " ".to_string(),
            vec![(t.steer_send_all, Control::SendAll)],
            width,
        ));
    }
    let max = lines
        .len()
        .saturating_sub(usize::from(layout.content.height));
    if state.steer.follow_selection {
        state.screen_scroll = crate::render::list_scroll_offset(
            lines.len(),
            layout.content.height.into(),
            selected_line,
        );
        state.steer.follow_selection = false;
    }
    state.screen_scroll = state.screen_scroll.min(max);
    paint(frame, layout.content, lines, state.screen_scroll, state);
}
