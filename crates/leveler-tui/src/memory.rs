//! `/memory` — the terminal page over durable cross-session memory.
//!
//! The page is a view over the runtime's memory API: it lists the two scopes,
//! previews one bounded file, and sends typed management requests. It never
//! opens the store, hashes a file for deletion, or invents a capture/dream
//! outcome — the runtime owns all of that, and `UiMemoryFileContent`'s hash is
//! the exact bytes the user saw.
//!
//! The layout is a searchable list (40%) beside a read-only preview (60%).
//! Under [`COLLAPSE_WIDTH`] the two panes collapse to one: Enter expands the
//! selected file over the full width and Esc returns to the list.

use std::time::{Duration, Instant};

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use leveler_client_protocol::{
    ClientCommand, CommandId, NotificationLevel, UiMemoryActionResult, UiMemoryFile,
    UiMemoryFileContent, UiMemoryScope, UiMemoryStatus,
};
use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};

use crate::action::Effect;
use crate::i18n::MemoryText;
use crate::screen::Screen;
use crate::state::{AppState, Notification};

/// How often the open page re-reads the runtime's counters. Capture and Dream
/// advance asynchronously, so a static page would show a stale queue.
const REFRESH_INTERVAL: Duration = Duration::from_secs(2);

/// The page collapses to one pane below this width. Two 30-column panes are
/// unusable; one full-width pane is not.
const COLLAPSE_WIDTH: u16 = 64;

/// A second `x` inside this window forgets. Beyond it the first press expires
/// and the next press only arms again.
const CONFIRM_FORGET_WINDOW: Duration = Duration::from_secs(4);

/// Fixed header rows in the list pane: scope tabs, filter, summary, blank.
const LIST_HEADER_ROWS: u16 = 4;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum MemoryScope {
    Global,
    #[default]
    Workspace,
}

impl MemoryScope {
    fn as_str(self) -> &'static str {
        match self {
            Self::Global => "global",
            Self::Workspace => "workspace",
        }
    }

    fn other(self) -> Self {
        match self {
            Self::Global => Self::Workspace,
            Self::Workspace => Self::Global,
        }
    }

    fn label(self, t: &MemoryText) -> &'static str {
        match self {
            Self::Global => t.scope_global,
            Self::Workspace => t.scope_workspace,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct PendingForget {
    scope: String,
    path: String,
    at: Instant,
}

/// The memory page's presentation state. Everything projected from the
/// runtime is replaced whole on each answer; nothing here is a second source
/// of truth for the store.
#[derive(Debug, Default)]
pub struct MemoryState {
    pub status: Option<UiMemoryStatus>,
    pub scope: MemoryScope,
    pub selected: usize,
    pub list_scroll: usize,
    pub filter: String,
    pub filter_editing: bool,
    pub preview: Option<UiMemoryFileContent>,
    pub preview_loading: bool,
    /// The selection the current preview was requested for, so an unchanged
    /// selection does not re-read the file every tick.
    preview_for: Option<(String, String)>,
    pub preview_scroll: usize,
    /// Wrapped preview rows, built once per (content, width).
    preview_rows: Vec<String>,
    preview_rows_generation: u64,
    preview_rows_width: usize,
    preview_generation: u64,
    pub status_query: Option<CommandId>,
    pub preview_query: Option<CommandId>,
    pub action_query: Option<CommandId>,
    pending_forget: Option<PendingForget>,
    pub notice: Option<String>,
    pub error: Option<String>,
    pub last_refresh: Option<Instant>,
    pub refresh_pending: bool,
    /// Narrow layout: the preview owns the full body.
    pub expanded: bool,
}

// ---------------------------------------------------------------------------
// Entry points
// ---------------------------------------------------------------------------

/// Open the page, or close it when it is already open.
pub fn open(state: &mut AppState) -> Vec<Effect> {
    if state.active_screen == Screen::Memory {
        close(state);
        return Vec::new();
    }
    state.memory = MemoryState::default();
    state.active_screen = Screen::Memory;
    state.screen_scroll = 0;
    state.memory.last_refresh = Some(Instant::now());
    vec![status_effect(state)]
}

fn close(state: &mut AppState) {
    state.active_screen = Screen::Conversation;
    state.screen_scroll = 0;
    state.memory.status_query = None;
    state.memory.preview_query = None;
    state.memory.action_query = None;
}

/// The queries the open page still owes: a counter refresh on a bounded
/// cadence and a preview for the current selection.
pub fn tail_queries(state: &mut AppState, now: Instant) -> Vec<Effect> {
    if state.active_screen != Screen::Memory {
        return Vec::new();
    }
    let mut effects = Vec::new();
    if let Some(pending) = state.memory.pending_forget.as_ref()
        && now.saturating_duration_since(pending.at) >= CONFIRM_FORGET_WINDOW
    {
        state.memory.pending_forget = None;
    }
    let due = state.memory.refresh_pending
        || state
            .memory
            .last_refresh
            .is_none_or(|last| now.saturating_duration_since(last) >= REFRESH_INTERVAL);
    if due && state.memory.status_query.is_none() {
        state.memory.refresh_pending = false;
        state.memory.last_refresh = Some(now);
        effects.push(status_effect(state));
    }
    if let Some(effect) = preview_effect(state) {
        effects.push(effect);
    }
    effects
}

// ---------------------------------------------------------------------------
// Runtime answers
// ---------------------------------------------------------------------------

/// Apply `MemoryLoaded`. A response for a query this page no longer owns is
/// ignored: another client's answer must not replace this page's state.
pub fn apply_status(
    state: &mut AppState,
    query_id: Option<&CommandId>,
    status: Box<UiMemoryStatus>,
) {
    if query_id.is_none() || state.memory.status_query.as_ref() != query_id {
        return;
    }
    state.memory.status_query = None;
    state.memory.status = Some(*status);
    clamp_selection(state);
}

/// Apply `MemoryFileLoaded`. The page keeps exactly one outstanding preview,
/// so a mismatched id is a stale answer and is dropped.
pub fn apply_file(state: &mut AppState, query_id: Option<&CommandId>, file: UiMemoryFileContent) {
    if query_id.is_none() || state.memory.preview_query.as_ref() != query_id {
        return;
    }
    state.memory.preview_query = None;
    state.memory.preview_loading = false;
    state.memory.preview_for = Some((file.scope.clone(), file.path.clone()));
    state.memory.preview_generation = state.memory.preview_generation.wrapping_add(1);
    state.memory.preview_scroll = 0;
    state.memory.preview = Some(file);
}

/// Apply a settled management action.
pub fn apply_action(
    state: &mut AppState,
    query_id: Option<&CommandId>,
    result: UiMemoryActionResult,
) {
    if query_id.is_none() || state.memory.action_query.as_ref() != query_id {
        return;
    }
    state.memory.action_query = None;
    let text = {
        let t = state.t();
        if result.ok {
            let label = match result.action.as_str() {
                "remember" => Some(t.memory.action_remember),
                "dream" => Some(t.memory.action_dream),
                "batch_dream" => Some(t.memory.action_batch_dream),
                "flush" => Some(t.memory.action_flush),
                "toggle" => Some(t.memory.action_toggle),
                _ => None,
            };
            match label {
                // A successful forget's message is a generic sentence; the
                // page has its own locale-correct line for it.
                _ if result.action == "forget" => t.memory.forgot.to_string(),
                Some(label) => format!("{label} · {}", result.message),
                None => result.message.clone(),
            }
        } else {
            format!("{} · {}", t.memory.action_failed, result.message)
        }
    };
    if state.active_screen == Screen::Memory {
        if result.ok {
            state.memory.notice = Some(text);
            state.memory.error = None;
        } else {
            // The page prepends its own localized "Error:" label; the runtime
            // message stays as the technical detail.
            state.memory.error = Some(result.message);
        }
    } else {
        state.notification = Some(Notification {
            level: if result.ok {
                NotificationLevel::Info
            } else {
                NotificationLevel::Warning
            },
            message: text,
        });
    }
    if !result.ok {
        return;
    }
    state.memory.refresh_pending = true;
    // Every management action can rewrite the manifest or the files around
    // the selection. Drop the preview so the page paints the store's current
    // bytes instead of the copy it read before the action settled.
    state.memory.preview = None;
    state.memory.preview_for = None;
    state.memory.preview_loading = false;
    if result.action == "forget" {
        state.memory.pending_forget = None;
        clamp_selection(state);
    }
}

// ---------------------------------------------------------------------------
// Actions
// ---------------------------------------------------------------------------

fn status_effect(state: &mut AppState) -> Effect {
    let query_id = CommandId::generate();
    state.memory.status_query = Some(query_id.clone());
    Effect::Send(ClientCommand::QueryMemory {
        session_id: state.session_id.clone(),
        query_id: Some(query_id),
    })
}

/// Ask for the selected file when the selection is not what the page already
/// holds or is waiting for.
fn preview_effect(state: &mut AppState) -> Option<Effect> {
    if state.memory.preview_query.is_some() {
        return None;
    }
    let (scope, path) = selected_key(state)?;
    if state.memory.preview_for.as_ref() == Some(&(scope.clone(), path.clone())) {
        return None;
    }
    let query_id = CommandId::generate();
    state.memory.preview_query = Some(query_id.clone());
    state.memory.preview_loading = true;
    state.memory.preview_for = Some((scope.clone(), path.clone()));
    Some(Effect::Send(ClientCommand::ReadMemoryFile {
        session_id: state.session_id.clone(),
        query_id: Some(query_id),
        scope,
        path,
    }))
}

fn dream_effect(state: &mut AppState, batch: bool) -> Vec<Effect> {
    if !begin_action(state) {
        return Vec::new();
    }
    let query_id = CommandId::generate();
    state.memory.action_query = Some(query_id.clone());
    state.memory.notice = Some(state.t().memory.action_pending.to_string());
    vec![Effect::Send(ClientCommand::RunMemoryDream {
        session_id: state.session_id.clone(),
        query_id: Some(query_id),
        batch,
    })]
}

fn toggle_effect(state: &mut AppState) -> Vec<Effect> {
    let t = state.t();
    let Some(status) = state.memory.status.as_ref() else {
        state.memory.error = Some(t.memory.loading.to_string());
        return Vec::new();
    };
    if status.rollout == "off" {
        state.memory.error = Some(t.memory.toggle_configured_off.to_string());
        return Vec::new();
    }
    let enabled = !status.enabled;
    if !begin_action(state) {
        return Vec::new();
    }
    let query_id = CommandId::generate();
    state.memory.action_query = Some(query_id.clone());
    state.memory.notice = Some(t.memory.action_pending.to_string());
    vec![Effect::Send(ClientCommand::ToggleMemory {
        session_id: state.session_id.clone(),
        query_id: Some(query_id),
        enabled,
    })]
}

fn flush_effect(state: &mut AppState) -> Vec<Effect> {
    if !begin_action(state) {
        return Vec::new();
    }
    let query_id = CommandId::generate();
    state.memory.action_query = Some(query_id.clone());
    state.memory.notice = Some(state.t().memory.action_pending.to_string());
    vec![Effect::Send(ClientCommand::FlushMemory {
        session_id: state.session_id.clone(),
        query_id: Some(query_id),
    })]
}

fn begin_action(state: &mut AppState) -> bool {
    if state.memory.action_query.is_some() {
        state.memory.error = Some(state.t().memory.action_busy.to_string());
        return false;
    }
    state.memory.error = None;
    true
}

/// `x`: arm the forget, and send it on a second press inside the window.
///
/// The page refuses anything it cannot bind to the exact bytes on screen: a
/// protected file, a preview that is still loading, a preview of another file,
/// or a truncated read (its hash is only the prefix's).
fn press_forget(state: &mut AppState) -> Vec<Effect> {
    let t = state.t();
    let Some(file) = selected_file(state) else {
        state.memory.error = Some(t.memory.empty.to_string());
        return Vec::new();
    };
    if !file.deletable {
        state.memory.error = Some(t.memory.protected.to_string());
        return Vec::new();
    }
    let scope = state.memory.scope.as_str().to_owned();
    let path = file.path.clone();
    let preview = state.memory.preview.as_ref().filter(|preview| {
        preview.error.is_none()
            && !preview.truncated
            && !preview.content_hash.is_empty()
            && preview.scope == scope
            && preview.path == path
    });
    let Some(preview) = preview else {
        state.memory.error = Some(t.memory.no_preview.to_string());
        return Vec::new();
    };
    let content_hash = preview.content_hash.clone();
    let now = Instant::now();
    let confirmed = state.memory.pending_forget.as_ref().is_some_and(|pending| {
        pending.scope == scope
            && pending.path == path
            && now.saturating_duration_since(pending.at) < CONFIRM_FORGET_WINDOW
    });
    if !confirmed {
        state.memory.pending_forget = Some(PendingForget {
            scope,
            path: path.clone(),
            at: now,
        });
        state.memory.error = None;
        state.memory.notice = Some(t.memory.confirm_forget.replacen("{}", &path, 1));
        return Vec::new();
    }
    if state.memory.action_query.is_some() {
        state.memory.error = Some(t.memory.action_busy.to_string());
        return Vec::new();
    }
    let query_id = CommandId::generate();
    state.memory.action_query = Some(query_id.clone());
    state.memory.pending_forget = None;
    state.memory.notice = Some(t.memory.forget_pending.to_string());
    state.memory.error = None;
    vec![Effect::Send(ClientCommand::ForgetMemory {
        session_id: state.session_id.clone(),
        query_id: Some(query_id),
        scope,
        path,
        content_hash,
    })]
}

fn copy_selected_path(state: &mut AppState) {
    let t = state.t();
    let Some(file) = selected_file(state) else {
        return;
    };
    let path = file.path.clone();
    let root = scope_summary(state).map(|summary| summary.root.clone());
    let absolute = match root {
        Some(root) if !root.is_empty() => format!("{root}/{path}"),
        _ => path,
    };
    match crate::selection::copy_to_clipboard(&absolute) {
        // The page owns both surfaces it paints: a global notification is not
        // visible on a secondary screen, so the outcome has to be page-local.
        Ok(_) => {
            state.memory.notice = Some(t.memory.copied.to_string());
            state.memory.error = None;
        }
        Err(message) => {
            state.memory.error = Some(message);
            state.memory.notice = None;
        }
    }
}

/// Re-read the authority after a manual refresh.
fn refresh(state: &mut AppState) -> Vec<Effect> {
    state.memory.last_refresh = Some(Instant::now());
    state.memory.preview_for = None;
    clamp_selection(state);
    vec![status_effect(state)]
}

fn set_selection(state: &mut AppState, index: usize) {
    let len = visible_len(state);
    if len == 0 {
        state.memory.selected = 0;
        return;
    }
    let next = index.min(len - 1);
    if next != state.memory.selected {
        state.memory.selected = next;
        state.memory.preview_scroll = 0;
    }
}

fn move_selection(state: &mut AppState, delta: isize) {
    let len = visible_len(state);
    if len == 0 {
        state.memory.selected = 0;
        return;
    }
    let current = state.memory.selected as isize;
    set_selection(state, (current + delta).clamp(0, len as isize - 1) as usize);
}

fn switch_scope(state: &mut AppState) {
    state.memory.scope = state.memory.scope.other();
    state.memory.selected = 0;
    state.memory.list_scroll = 0;
    state.memory.preview_scroll = 0;
    state.memory.filter_editing = false;
    state.memory.pending_forget = None;
}

fn clamp_selection(state: &mut AppState) {
    let len = visible_len(state);
    if len == 0 {
        state.memory.selected = 0;
    } else if state.memory.selected >= len {
        state.memory.selected = len - 1;
    }
}

// ---------------------------------------------------------------------------
// Keys
// ---------------------------------------------------------------------------

pub fn handle_key(state: &mut AppState, key: &KeyEvent) -> Vec<Effect> {
    if state.memory.filter_editing {
        return handle_filter_key(state, key);
    }
    // A modified key is never a plain page command. Ctrl+F is the reader's
    // fullscreen toggle; without this gate it would reach the `f` flush arm,
    // and Ctrl+D would run a Dream.
    if key.modifiers.contains(KeyModifiers::CONTROL) {
        if key.code == KeyCode::Char('f') {
            toggle_fullscreen(state);
        }
        return Vec::new();
    }
    if state.memory.expanded {
        return handle_preview_key(state, key);
    }
    handle_list_key(state, key)
}

/// Ctrl+F and Enter open the selected file over the whole body; the same key
/// (or Esc) returns to the split view.
fn toggle_fullscreen(state: &mut AppState) {
    if state.memory.expanded {
        state.memory.expanded = false;
        state.memory.preview_scroll = 0;
    } else if selected_file(state).is_some() {
        state.memory.expanded = true;
        state.memory.preview_scroll = 0;
    }
}

fn handle_filter_key(state: &mut AppState, key: &KeyEvent) -> Vec<Effect> {
    match key.code {
        KeyCode::Esc => {
            state.memory.filter_editing = false;
        }
        KeyCode::Enter => {
            state.memory.filter_editing = false;
        }
        KeyCode::Backspace => {
            state.memory.filter.pop();
            state.memory.selected = 0;
            state.memory.list_scroll = 0;
        }
        KeyCode::Char('u')
            if key
                .modifiers
                .contains(crossterm::event::KeyModifiers::CONTROL) =>
        {
            state.memory.filter.clear();
            state.memory.selected = 0;
        }
        KeyCode::Char(character)
            if !key
                .modifiers
                .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
        {
            state.memory.filter.push(character);
            state.memory.selected = 0;
            state.memory.list_scroll = 0;
        }
        _ => {}
    }
    Vec::new()
}

fn handle_list_key(state: &mut AppState, key: &KeyEvent) -> Vec<Effect> {
    match key.code {
        KeyCode::Esc => {
            if !state.memory.filter.is_empty() {
                state.memory.filter.clear();
                state.memory.selected = 0;
            } else {
                back(state);
            }
            Vec::new()
        }
        KeyCode::Up | KeyCode::Char('k') => {
            move_selection(state, -1);
            Vec::new()
        }
        KeyCode::Down | KeyCode::Char('j') => {
            move_selection(state, 1);
            Vec::new()
        }
        KeyCode::Left | KeyCode::Char('h') => {
            switch_to_scope(state, MemoryScope::Global);
            Vec::new()
        }
        KeyCode::Right | KeyCode::Char('l') => {
            switch_to_scope(state, MemoryScope::Workspace);
            Vec::new()
        }
        KeyCode::Tab | KeyCode::BackTab => {
            switch_scope(state);
            Vec::new()
        }
        KeyCode::Char('/') => {
            state.memory.filter_editing = true;
            Vec::new()
        }
        KeyCode::Enter => {
            if selected_file(state).is_some() {
                state.memory.expanded = true;
                state.memory.preview_scroll = 0;
            }
            Vec::new()
        }
        KeyCode::Char('x') => press_forget(state),
        KeyCode::Char('y') => {
            copy_selected_path(state);
            Vec::new()
        }
        KeyCode::Char('r') => refresh(state),
        KeyCode::Char('t') => toggle_effect(state),
        KeyCode::Char('d') => dream_effect(state, false),
        KeyCode::Char('D') => dream_effect(state, true),
        KeyCode::Char('f') => flush_effect(state),
        KeyCode::PageUp => {
            state.memory.preview_scroll = state.memory.preview_scroll.saturating_sub(10);
            Vec::new()
        }
        KeyCode::PageDown => {
            state.memory.preview_scroll = state.memory.preview_scroll.saturating_add(10);
            Vec::new()
        }
        _ => Vec::new(),
    }
}

fn switch_to_scope(state: &mut AppState, scope: MemoryScope) {
    if state.memory.scope == scope {
        return;
    }
    state.memory.scope = scope;
    state.memory.selected = 0;
    state.memory.list_scroll = 0;
    state.memory.preview_scroll = 0;
    state.memory.filter_editing = false;
    state.memory.pending_forget = None;
}

/// Narrow layout: the preview owns the body, so the list's keys scroll it.
fn handle_preview_key(state: &mut AppState, key: &KeyEvent) -> Vec<Effect> {
    match key.code {
        KeyCode::Esc | KeyCode::Enter | KeyCode::Left => {
            state.memory.expanded = false;
            state.memory.preview_scroll = 0;
        }
        KeyCode::Up | KeyCode::Char('k') => {
            state.memory.preview_scroll = state.memory.preview_scroll.saturating_sub(1);
        }
        KeyCode::Down | KeyCode::Char('j') => {
            state.memory.preview_scroll = state.memory.preview_scroll.saturating_add(1);
        }
        KeyCode::PageUp => {
            state.memory.preview_scroll = state.memory.preview_scroll.saturating_sub(10);
        }
        KeyCode::PageDown => {
            state.memory.preview_scroll = state.memory.preview_scroll.saturating_add(10);
        }
        KeyCode::Home | KeyCode::Char('g') => state.memory.preview_scroll = 0,
        KeyCode::End | KeyCode::Char('G') => {
            state.memory.preview_scroll = usize::MAX / 2;
        }
        KeyCode::Char('x') => return press_forget(state),
        KeyCode::Char('y') => copy_selected_path(state),
        KeyCode::Char('r') => return refresh(state),
        KeyCode::Char('t') => return toggle_effect(state),
        KeyCode::Char('d') => return dream_effect(state, false),
        KeyCode::Char('D') => return dream_effect(state, true),
        KeyCode::Char('f') => return flush_effect(state),
        _ => {}
    }
    Vec::new()
}

/// One level back: narrowed preview → list → conversation.
pub fn back(state: &mut AppState) {
    if state.memory.expanded {
        state.memory.expanded = false;
        state.memory.preview_scroll = 0;
        return;
    }
    close(state);
}

// ---------------------------------------------------------------------------
// Mouse
// ---------------------------------------------------------------------------

/// A wheel notch moves the list cursor; in the narrowed preview it scrolls the
/// document three rows.
pub fn wheel(state: &mut AppState, notches: isize) {
    if state.memory.expanded {
        let lines = (notches * 3).unsigned_abs();
        state.memory.preview_scroll = if notches < 0 {
            state.memory.preview_scroll.saturating_sub(lines)
        } else {
            state.memory.preview_scroll.saturating_add(lines)
        };
    } else {
        move_selection(state, notches);
    }
}

/// A click on a painted list row, keyed by its index in the filtered list.
pub fn select_row(state: &mut AppState, key: &str) {
    if state.memory.expanded {
        return;
    }
    if let Ok(index) = key.parse::<usize>() {
        set_selection(state, index);
    }
}

// ---------------------------------------------------------------------------
// Selection projection
// ---------------------------------------------------------------------------

fn scope_summary(state: &AppState) -> Option<&UiMemoryScope> {
    let status = state.memory.status.as_ref()?;
    Some(match state.memory.scope {
        MemoryScope::Global => &status.global,
        MemoryScope::Workspace => &status.workspace,
    })
}

fn matches_filter(file: &UiMemoryFile, filter: &str) -> bool {
    if filter.is_empty() {
        return true;
    }
    let needle = filter.to_lowercase();
    file.path.to_lowercase().contains(&needle) || file.kind.to_lowercase().contains(&needle)
}

fn visible_files<'a>(
    summary: &'a UiMemoryScope,
    filter: &'a str,
) -> impl Iterator<Item = &'a UiMemoryFile> + 'a {
    summary
        .files
        .iter()
        .filter(move |file| matches_filter(file, filter))
}

fn visible_len(state: &AppState) -> usize {
    match scope_summary(state) {
        Some(summary) => visible_files(summary, &state.memory.filter).count(),
        None => 0,
    }
}

fn selected_file(state: &AppState) -> Option<&UiMemoryFile> {
    let summary = scope_summary(state)?;
    visible_files(summary, &state.memory.filter).nth(state.memory.selected)
}

fn selected_key(state: &AppState) -> Option<(String, String)> {
    let file = selected_file(state)?;
    Some((state.memory.scope.as_str().to_owned(), file.path.clone()))
}

// ---------------------------------------------------------------------------
// Rendering
// ---------------------------------------------------------------------------

pub fn render_screen(frame: &mut Frame, area: Rect, state: &mut AppState) {
    let t = state.t();
    let hint = if state.memory.filter_editing {
        t.memory.footer_filter
    } else if state.memory.expanded {
        t.memory.footer_preview
    } else {
        t.memory.footer_list
    };
    let page = crate::secondary::SecondaryPage {
        title: t.memory.title,
        status: crate::secondary::main_status_spans(state, area.width as usize),
        hint,
    };
    let footer_rows = hint.lines().count().max(1) as u16;
    let layout = crate::secondary::layout_with_footer(area, 0, footer_rows);
    crate::secondary::draw_header(frame, state, &layout, &page);
    crate::secondary::draw_footer(frame, &layout, &page, &state.theme);
    let body = layout.content;
    if body.width == 0 || body.height == 0 {
        state.screen_rows.clear();
        return;
    }
    // Ctrl+F/Enter gives the selected file the whole body at any width; the
    // split layout is the default, not the only reading mode.
    if state.memory.expanded {
        state.screen_rows.clear();
        render_preview(frame, body, state);
        return;
    }
    if body.width < COLLAPSE_WIDTH {
        render_list(frame, body, state);
        return;
    }
    let chunks =
        Layout::horizontal([Constraint::Percentage(40), Constraint::Percentage(60)]).split(body);
    render_list(frame, chunks[0], state);
    render_preview(frame, chunks[1], state);
}

fn render_list(frame: &mut Frame, area: Rect, state: &mut AppState) {
    state.screen_rows.clear();
    if area.width == 0 || area.height == 0 {
        return;
    }
    let width = area.width as usize;
    let theme = state.theme.clone();
    let t = state.t();
    let mut lines: Vec<Line<'static>> = vec![
        scope_tabs(state, width),
        filter_line(state, width),
        summary_line(state, width),
        Line::default(),
    ];

    let len = visible_len(state);
    let visible_rows = (area.height.saturating_sub(LIST_HEADER_ROWS)) as usize;
    if state.memory.selected >= len {
        state.memory.selected = len.saturating_sub(1);
    }
    if state.memory.selected < state.memory.list_scroll {
        state.memory.list_scroll = state.memory.selected;
    } else if visible_rows > 0 && state.memory.selected >= state.memory.list_scroll + visible_rows {
        state.memory.list_scroll = state.memory.selected + 1 - visible_rows;
    }

    if state.memory.status.is_none() {
        lines.push(muted(t.memory.loading, &theme));
    } else if state
        .memory
        .status
        .as_ref()
        .is_some_and(|status| !status.enabled)
    {
        let status = state.memory.status.as_ref().expect("checked above");
        let text = if status.rollout == "off" {
            t.memory.off
        } else {
            t.memory.paused
        };
        push_wrapped(&mut lines, text, theme.text.muted, width);
    } else if scope_summary(state).is_some_and(|scope| scope.error.is_some()) {
        let error = scope_summary(state)
            .and_then(|scope| scope.error.clone())
            .unwrap_or_default();
        push_wrapped(
            &mut lines,
            &t.memory.error_label.replacen("{}", &error, 1),
            theme.status.error,
            width,
        );
    } else if len == 0 {
        let text = if state.memory.filter.is_empty() {
            t.memory.empty
        } else {
            t.memory.empty_filter
        };
        push_wrapped(&mut lines, text, theme.text.muted, width);
    } else {
        let summary = scope_summary(state);
        if let Some(summary) = summary {
            let skip = state.memory.list_scroll;
            let start = lines.len();
            let mut hits: Vec<(Rect, String)> = Vec::new();
            for (index, file) in visible_files(summary, &state.memory.filter)
                .enumerate()
                .skip(skip)
                .take(visible_rows)
            {
                let selected = index == state.memory.selected;
                let row = file_row(
                    file,
                    selected && !state.memory.filter_editing,
                    width,
                    &t.memory,
                    &theme,
                );
                let y = area.y + (start as u16) + (index - skip) as u16;
                if y < area.y + area.height {
                    hits.push((Rect::new(area.x, y, area.width, 1), index.to_string()));
                }
                lines.push(row);
            }
            state.screen_rows = hits;
        }
    }
    if let Some(error) = state.memory.error.clone() {
        push_wrapped(
            &mut lines,
            &t.memory.error_label.replacen("{}", &error, 1),
            theme.status.error,
            width,
        );
    } else if let Some(notice) = state.memory.notice.clone() {
        push_wrapped(&mut lines, &notice, theme.status.success, width);
    }
    let paragraph = ratatui::widgets::Paragraph::new(lines);
    frame.render_widget(paragraph, area);
}

fn scope_tabs(state: &AppState, width: usize) -> Line<'static> {
    let theme = &state.theme;
    let t = state.t();
    let mut spans: Vec<Span<'static>> = Vec::new();
    for (index, scope) in [MemoryScope::Global, MemoryScope::Workspace]
        .into_iter()
        .enumerate()
    {
        if index > 0 {
            spans.push(Span::styled("  ", Style::default()));
        }
        let active = state.memory.scope == scope;
        let count = state
            .memory
            .status
            .as_ref()
            .map(|status| match scope {
                MemoryScope::Global => status.global.files.len(),
                MemoryScope::Workspace => status.workspace.files.len(),
            })
            .unwrap_or(0);
        let text = format!(
            "{} {} {}",
            if active { "●" } else { "○" },
            scope.label(&t.memory),
            count
        );
        let style = if active {
            Style::default()
                .fg(theme.accent.primary)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(theme.text.muted)
        };
        spans.push(Span::styled(
            crate::render::truncate_display(&text, width),
            style,
        ));
    }
    Line::from(spans)
}

fn filter_line(state: &AppState, width: usize) -> Line<'static> {
    let theme = &state.theme;
    let t = state.t();
    let text = if state.memory.filter_editing {
        t.memory
            .filter_label
            .replacen("{}", &state.memory.filter, 1)
    } else if state.memory.filter.is_empty() {
        t.memory.filter_placeholder.to_string()
    } else {
        t.memory
            .filter_label
            .replacen("{}", &state.memory.filter, 1)
    };
    let style = if state.memory.filter_editing {
        Style::default().fg(theme.accent.primary)
    } else if state.memory.filter.is_empty() {
        Style::default().fg(theme.text.muted)
    } else {
        Style::default().fg(theme.text.primary)
    };
    Line::from(Span::styled(
        crate::render::truncate_display(&text, width),
        style,
    ))
}

fn summary_line(state: &AppState, width: usize) -> Line<'static> {
    let theme = &state.theme;
    let t = state.t();
    let Some(scope) = scope_summary(state) else {
        return Line::default();
    };
    if scope.error.is_some() {
        return Line::default();
    }
    let visible = visible_files(scope, &state.memory.filter).count();
    let mut text = t
        .memory
        .summary
        .replacen("{}", &visible.to_string(), 1)
        .replacen("{}", &scope.pending_observations.to_string(), 1)
        .replacen("{}", &scope.archive_count.to_string(), 1)
        .replacen("{}", &scope.tombstone_count.to_string(), 1);
    if scope.pending_capture_jobs
        + scope.running_capture_jobs
        + scope.failed_capture_jobs
        + scope.retry_capture_jobs
        > 0
    {
        text.push_str(" · ");
        text.push_str(
            &t.memory
                .captures
                .replacen("{}", &scope.pending_capture_jobs.to_string(), 1)
                .replacen("{}", &scope.running_capture_jobs.to_string(), 1)
                .replacen("{}", &scope.failed_capture_jobs.to_string(), 1),
        );
    }
    if state
        .memory
        .status
        .as_ref()
        .is_some_and(|status| status.workspace_ephemeral)
        && state.memory.scope == MemoryScope::Workspace
    {
        text.push_str(" · ");
        text.push_str(t.memory.ephemeral);
    }
    Line::from(Span::styled(
        crate::render::truncate_display(&text, width),
        Style::default().fg(theme.text.muted),
    ))
}

/// `MM-DD HH:MM` in local time.
const UPDATED_W: usize = 11;

fn updated_label(modified_at: Option<i64>) -> String {
    modified_at
        .and_then(|seconds| chrono::DateTime::from_timestamp(seconds, 0))
        .map(|time| {
            time.with_timezone(&chrono::Local)
                .format("%m-%d %H:%M")
                .to_string()
        })
        .unwrap_or_else(|| "-".to_owned())
}

fn file_row(
    file: &UiMemoryFile,
    selected: bool,
    width: usize,
    t: &MemoryText,
    theme: &crate::theme::Theme,
) -> Line<'static> {
    let marker = if selected { "› " } else { "  " };
    let kind = kind_label(&file.kind, t);
    let size = human_bytes(file.bytes);
    let updated = updated_label(file.modified_at);
    let kind_w = display_width(kind).max(4);
    // The size renders in an eight-column field even when the text is shorter.
    let size_w = size.len().max(8);
    let fixed = 2 + kind_w + 1 + 1 + UPDATED_W + 1 + size_w;
    let path_w = width.saturating_sub(fixed).max(1);
    let path = crate::render::truncate_display(&file.path, path_w);
    let style = if selected {
        Style::default().fg(theme.accent.primary)
    } else {
        Style::default().fg(theme.text.primary)
    };
    let kind_style = if file.deletable {
        Style::default().fg(theme.text.muted)
    } else {
        Style::default()
            .fg(theme.text.muted)
            .add_modifier(Modifier::DIM)
    };
    Line::from(vec![
        Span::styled(marker, style),
        Span::styled(crate::render::pad_display(kind, kind_w), kind_style),
        Span::styled(" ", Style::default()),
        Span::styled(path, style),
        Span::styled(
            format!(" {updated:>UPDATED_W$}"),
            Style::default().fg(theme.text.muted),
        ),
        Span::styled(format!(" {size:>8}"), Style::default().fg(theme.text.muted)),
    ])
}

fn kind_label<'a>(kind: &str, t: &'a MemoryText) -> &'a str {
    match kind {
        "manifest" => t.kind_manifest,
        "topic" => t.kind_topic,
        "observation" => t.kind_observation,
        "archive" => t.kind_archive,
        _ => t.kind_other,
    }
}

fn display_width(text: &str) -> usize {
    unicode_width::UnicodeWidthStr::width(text)
}

fn muted(text: &str, theme: &crate::theme::Theme) -> Line<'static> {
    Line::from(Span::styled(
        text.to_string(),
        Style::default().fg(theme.text.muted),
    ))
}

fn push_wrapped(
    lines: &mut Vec<Line<'static>>,
    text: &str,
    color: ratatui::style::Color,
    width: usize,
) {
    for row in crate::render::wrap(&crate::render::sanitize_terminal_line(text), width.max(1)) {
        lines.push(Line::from(Span::styled(row, Style::default().fg(color))));
    }
}

fn render_preview(frame: &mut Frame, area: Rect, state: &mut AppState) {
    if area.width == 0 || area.height == 0 {
        return;
    }
    let width = area.width as usize;
    let theme = state.theme.clone();
    let t = state.t();
    let mut lines: Vec<Line<'static>> = Vec::new();
    let Some(file) = state.memory.preview.clone() else {
        let text = if state.memory.preview_loading {
            t.memory.preview_loading
        } else {
            t.memory.no_preview
        };
        frame.render_widget(
            ratatui::widgets::Paragraph::new(vec![muted(text, &theme)]),
            area,
        );
        return;
    };
    let title = format!("{} · {}", file.path, t.memory.read_only);
    lines.push(Line::from(Span::styled(
        crate::render::truncate_display(&title, width),
        Style::default()
            .fg(theme.accent.primary)
            .add_modifier(Modifier::BOLD),
    )));
    let mut meta = format!("{} · {}", human_bytes(file.total_bytes), file.path);
    if !file.content_hash.is_empty() {
        meta.push_str(" · ");
        meta.push_str(
            &t.memory
                .hash_label
                .replacen("{}", short_hash(&file.content_hash), 1),
        );
    }
    lines.push(Line::from(Span::styled(
        crate::render::truncate_display(&meta, width),
        Style::default().fg(theme.text.muted),
    )));
    if file.truncated {
        push_wrapped(
            &mut lines,
            &t.memory
                .truncated
                .replacen("{}", &file.total_bytes.to_string(), 1),
            theme.status.warning,
            width,
        );
    }
    if let Some(error) = &file.error {
        push_wrapped(
            &mut lines,
            &t.memory.error_label.replacen("{}", error, 1),
            theme.status.error,
            width,
        );
    } else {
        let generation = state.memory.preview_generation;
        if state.memory.preview_rows_generation != generation
            || state.memory.preview_rows_width != width
        {
            state.memory.preview_rows = crate::render::wrap(&file.content, width.max(1));
            state.memory.preview_rows_generation = generation;
            state.memory.preview_rows_width = width;
        }
        let available = (area.height as usize).saturating_sub(lines.len());
        let total = state.memory.preview_rows.len();
        let max_scroll = total.saturating_sub(available);
        if state.memory.preview_scroll > max_scroll {
            state.memory.preview_scroll = max_scroll;
        }
        for row in state
            .memory
            .preview_rows
            .iter()
            .skip(state.memory.preview_scroll)
            .take(available)
        {
            lines.push(Line::from(Span::styled(
                row.clone(),
                Style::default().fg(theme.text.primary),
            )));
        }
    }
    frame.render_widget(ratatui::widgets::Paragraph::new(lines), area);
}

fn short_hash(hash: &str) -> &str {
    hash.get(..16).unwrap_or(hash)
}

fn human_bytes(bytes: u64) -> String {
    const KIB: f64 = 1024.0;
    let value = bytes as f64;
    if value >= KIB * KIB {
        format!("{:.1}M", value / (KIB * KIB))
    } else if value >= KIB {
        format!("{:.1}K", value / KIB)
    } else {
        format!("{bytes}B")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::i18n::Locale;
    use crate::state::Boot;
    use crate::theme::Theme;
    use leveler_client_protocol::{SessionId, UiMemoryActivity, UiMemorySession};

    fn state() -> AppState {
        AppState::new(
            Theme::no_color(),
            Boot {
                session_id: SessionId::new("tui-memory"),
                user: "u".into(),
                version: "0".into(),
                show_welcome: false,
                draft_path: None,
                history_path: None,
                context_window: 0,
                locale: Locale::Zh,
                untrusted_config: Vec::new(),
                model_notice: None,
                thinking: None,
            },
        )
    }

    fn file(scope: &str, path: &str, deletable: bool) -> UiMemoryFile {
        UiMemoryFile {
            scope: scope.to_owned(),
            path: path.to_owned(),
            kind: "topic".to_owned(),
            bytes: 10,
            modified_at: None,
            deletable,
        }
    }

    fn memory_status() -> UiMemoryStatus {
        UiMemoryStatus {
            rollout: "active".to_owned(),
            enabled: true,
            capture_enabled: true,
            automatic_dream_enabled: true,
            manual_dream_enabled: true,
            batch_dream_enabled: true,
            workspace_ephemeral: false,
            global: UiMemoryScope {
                scope: "global".to_owned(),
                root: "/mem/global".to_owned(),
                files: vec![file("global", "topics/editor.md", true)],
                ..Default::default()
            },
            workspace: UiMemoryScope {
                scope: "workspace".to_owned(),
                root: "/mem/workspace".to_owned(),
                files: vec![
                    file("workspace", "MEMORY.md", false),
                    file("workspace", "topics/build.md", true),
                    file("workspace", "topics/cache.md", true),
                ],
                ..Default::default()
            },
            session: Some(UiMemorySession::default()),
            activity: UiMemoryActivity::default(),
        }
    }

    fn state_with_status() -> AppState {
        let mut state = state();
        state.memory.status = Some(memory_status());
        state.memory.scope = MemoryScope::Workspace;
        state
    }

    #[test]
    fn the_filter_narrows_by_path_and_keeps_selection_in_range() {
        let mut state = state_with_status();
        assert_eq!(visible_len(&state), 3);
        state.memory.selected = 2;
        state.memory.filter = "cache".to_owned();
        clamp_selection(&mut state);
        assert_eq!(visible_len(&state), 1);
        assert_eq!(state.memory.selected, 0);
        assert_eq!(
            selected_file(&state).map(|file| file.path.as_str()),
            Some("topics/cache.md")
        );
    }

    #[test]
    fn switching_scope_resets_the_cursor_and_filter_mode() {
        let mut state = state_with_status();
        state.memory.selected = 2;
        state.memory.filter_editing = true;
        switch_scope(&mut state);
        assert_eq!(state.memory.scope, MemoryScope::Global);
        assert_eq!(state.memory.selected, 0);
        assert!(!state.memory.filter_editing);
        assert_eq!(visible_len(&state), 1);
    }

    #[test]
    fn a_response_for_a_query_the_page_no_longer_owns_is_ignored() {
        let mut state = state_with_status();
        let current = CommandId::generate();
        let stale = CommandId::generate();
        state.memory.status_query = Some(current.clone());
        let mut replacement = memory_status();
        replacement.rollout = "off".to_owned();
        apply_status(&mut state, Some(&stale), Box::new(replacement));
        assert_eq!(
            state
                .memory
                .status
                .as_ref()
                .map(|status| status.rollout.as_str()),
            Some("active")
        );
        assert!(state.memory.status_query.is_some());
    }

    #[test]
    fn ctrl_f_toggles_the_fullscreen_reader_without_running_flush() {
        let mut state = state_with_status();
        let key = KeyEvent::new(KeyCode::Char('f'), KeyModifiers::CONTROL);
        let opened = handle_key(&mut state, &key);
        assert!(opened.is_empty(), "Ctrl+F must not run the flush action");
        assert!(state.memory.expanded);
        assert!(state.memory.action_query.is_none());
        let closed = handle_key(&mut state, &key);
        assert!(closed.is_empty());
        assert!(!state.memory.expanded);
    }

    #[test]
    fn t_pauses_memory_for_the_session() {
        let mut state = state_with_status();
        let effects = handle_key(
            &mut state,
            &KeyEvent::new(KeyCode::Char('t'), KeyModifiers::NONE),
        );
        assert_eq!(effects.len(), 1);
        let Effect::Send(ClientCommand::ToggleMemory { enabled, .. }) = &effects[0] else {
            panic!("t must send a memory toggle, got {effects:?}");
        };
        assert!(!enabled, "memory on → the toggle turns it off");
        assert!(state.memory.action_query.is_some());
    }

    #[test]
    fn t_resumes_a_paused_session() {
        let mut state = state_with_status();
        let mut paused = memory_status();
        paused.enabled = false;
        paused.capture_enabled = false;
        state.memory.status = Some(paused);
        let effects = handle_key(
            &mut state,
            &KeyEvent::new(KeyCode::Char('t'), KeyModifiers::NONE),
        );
        assert_eq!(effects.len(), 1);
        let Effect::Send(ClientCommand::ToggleMemory { enabled, .. }) = &effects[0] else {
            panic!("t must send a memory toggle, got {effects:?}");
        };
        assert!(enabled, "memory paused → the toggle turns it on");
    }

    #[test]
    fn t_is_refused_when_the_configuration_disables_memory() {
        let mut state = state_with_status();
        let mut off = memory_status();
        off.rollout = "off".to_owned();
        off.enabled = false;
        state.memory.status = Some(off);
        let effects = handle_key(
            &mut state,
            &KeyEvent::new(KeyCode::Char('t'), KeyModifiers::NONE),
        );
        assert!(
            effects.is_empty(),
            "an off configuration cannot be promoted by the page"
        );
        assert!(state.memory.error.is_some());
        assert!(state.memory.action_query.is_none());
    }

    #[test]
    fn the_first_x_arms_and_a_second_x_inside_the_window_sends_forget() {
        let mut state = state_with_status();
        state.memory.selected = 1;
        let selected = selected_file(&state).unwrap().clone();
        state.memory.preview = Some(UiMemoryFileContent {
            scope: selected.scope.clone(),
            path: selected.path.clone(),
            content: "note".to_owned(),
            content_hash: "a".repeat(64),
            truncated: false,
            total_bytes: 4,
            error: None,
        });
        let first = press_forget(&mut state);
        assert!(first.is_empty());
        assert!(state.memory.action_query.is_none());
        assert!(state.memory.notice.is_some());
        let second = press_forget(&mut state);
        assert_eq!(second.len(), 1);
        assert!(state.memory.action_query.is_some());
    }

    #[test]
    fn a_protected_manifest_cannot_be_forgotten() {
        let mut state = state_with_status();
        state.memory.selected = 0; // MEMORY.md
        let effects = press_forget(&mut state);
        assert!(effects.is_empty());
        assert!(state.memory.error.is_some());
        assert!(state.memory.pending_forget.is_none());
    }

    #[test]
    fn a_truncated_preview_is_not_a_forget_hash() {
        let mut state = state_with_status();
        state.memory.selected = 1;
        let selected = selected_file(&state).unwrap().clone();
        state.memory.preview = Some(UiMemoryFileContent {
            scope: selected.scope.clone(),
            path: selected.path.clone(),
            content: "note".to_owned(),
            content_hash: String::new(),
            truncated: true,
            total_bytes: 20,
            error: None,
        });
        let effects = press_forget(&mut state);
        assert!(effects.is_empty());
        assert!(state.memory.pending_forget.is_none());
    }

    #[test]
    fn a_forget_result_clears_the_preview_and_requests_a_refresh() {
        let mut state = state_with_status();
        state.active_screen = Screen::Memory;
        let query_id = CommandId::generate();
        state.memory.action_query = Some(query_id.clone());
        apply_action(
            &mut state,
            Some(&query_id),
            UiMemoryActionResult {
                action: "forget".to_owned(),
                ok: true,
                message: "Forgot.".to_owned(),
            },
        );
        assert!(state.memory.preview.is_none());
        assert!(state.memory.refresh_pending);
        assert!(state.memory.action_query.is_none());
        assert_eq!(
            state.memory.notice.as_deref(),
            Some(state.t().memory.forgot),
            "the page's own line replaces the runtime's English sentence"
        );
    }

    #[test]
    fn a_successful_action_keeps_its_runtime_detail_behind_a_localized_label() {
        let mut state = state();
        let query_id = CommandId::generate();
        state.memory.action_query = Some(query_id.clone());
        apply_action(
            &mut state,
            Some(&query_id),
            UiMemoryActionResult {
                action: "remember".to_owned(),
                ok: true,
                message: "Saved to global memory: note.md".to_owned(),
            },
        );
        let message = state.notification.as_ref().map(|n| n.message.as_str());
        assert!(
            message.is_some_and(|message| message.starts_with(state.t().memory.action_remember)),
            "{message:?}"
        );
        assert!(message.is_some_and(|message| message.contains("note.md")));
    }

    #[test]
    fn a_successful_management_action_rereads_the_preview() {
        // Dream, batch Dream and flush all rewrite the manifest the preview
        // was read from. Keeping the old bytes on screen would show memory
        // the store has already consolidated.
        let mut state = state_with_status();
        state.active_screen = Screen::Memory;
        state.memory.selected = 1;
        let selected = selected_file(&state).unwrap().clone();
        state.memory.preview = Some(UiMemoryFileContent {
            scope: selected.scope.clone(),
            path: selected.path.clone(),
            content: "stale".to_owned(),
            content_hash: "b".repeat(64),
            truncated: false,
            total_bytes: 5,
            error: None,
        });
        state.memory.preview_for = Some((selected.scope.clone(), selected.path.clone()));
        let query_id = CommandId::generate();
        state.memory.action_query = Some(query_id.clone());
        apply_action(
            &mut state,
            Some(&query_id),
            UiMemoryActionResult {
                action: "dream".to_owned(),
                ok: true,
                message: "Dream consolidated 2 observations into 1 topic.".to_owned(),
            },
        );
        assert!(state.memory.refresh_pending);
        assert!(
            state.memory.preview.is_none(),
            "the preview must not keep bytes the action invalidated"
        );
        assert!(
            state.memory.preview_for.is_none(),
            "the page must ask the store for the file again"
        );
    }

    #[test]
    fn a_file_row_shows_when_the_store_last_wrote_the_file() {
        let t = state().t();
        let theme = Theme::no_color();
        let mut topic = file("workspace", "topics/build.md", true);
        topic.modified_at = Some(1_700_000_000);
        let row = file_row(&topic, false, 80, &t.memory, &theme);
        let text: String = row.spans.iter().map(|span| span.content.as_ref()).collect();
        let expected = chrono::DateTime::from_timestamp(1_700_000_000, 0)
            .unwrap()
            .with_timezone(&chrono::Local)
            .format("%m-%d %H:%M")
            .to_string();
        assert!(
            text.contains(&expected),
            "the row must show the updated time {expected:?}: {text}"
        );
    }

    #[test]
    fn a_file_row_fits_the_width_it_is_given() {
        // The size is rendered in an 8-column field, so a three-character
        // size must not shrink the path budget by nothing.
        let t = state().t();
        let theme = Theme::no_color();
        let topic = file("workspace", "topics/build.md", true);
        for width in [30, 40, 60] {
            let row = file_row(&topic, true, width, &t.memory, &theme);
            let text: String = row.spans.iter().map(|span| span.content.as_ref()).collect();
            assert!(
                unicode_width::UnicodeWidthStr::width(text.as_str()) <= width,
                "row wider than {width}: {text:?}"
            );
        }
    }

    #[test]
    fn a_file_row_without_a_modified_time_shows_a_placeholder() {
        let t = state().t();
        let theme = Theme::no_color();
        let topic = file("workspace", "topics/build.md", true);
        let row = file_row(&topic, false, 80, &t.memory, &theme);
        let text: String = row.spans.iter().map(|span| span.content.as_ref()).collect();
        assert!(
            text.contains("          - "),
            "the updated column keeps its width with no time: {text:?}"
        );
    }

    #[test]
    fn a_failed_action_is_shown_as_an_error_not_a_success_notice() {
        let mut state = state_with_status();
        state.active_screen = Screen::Memory;
        let query_id = CommandId::generate();
        state.memory.action_query = Some(query_id.clone());
        apply_action(
            &mut state,
            Some(&query_id),
            UiMemoryActionResult {
                action: "dream".to_owned(),
                ok: false,
                message: "dream failed".to_owned(),
            },
        );
        assert_eq!(state.memory.error.as_deref(), Some("dream failed"));
        assert!(state.memory.notice.is_none());
        assert!(!state.memory.refresh_pending);
    }
}
