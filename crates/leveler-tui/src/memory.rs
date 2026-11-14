//! `/memory` — a centered overlay over the conversation.
//!
//! The page is a view over the runtime's memory API: it lists both scopes in
//! one grouped list, previews one bounded file, and sends typed management
//! requests. It never opens the store, hashes a file for deletion, or invents a
//! capture/dream outcome — the runtime owns all of that, and
//! `UiMemoryFileContent`'s hash is the exact bytes the user saw.
//!
//! The overlay is a searchable list (`GLOBAL` and `WORKSPACE` grouped in one
//! column, ~40%) beside a read-only Markdown preview (~60%). Ctrl+F fills the
//! terminal with the same state; Enter gives the preview the whole body. Below
//! `SPLIT_MIN_WIDTH` the panes collapse to the list, and Enter then reads the
//! selected file.

use std::time::{Duration, Instant};

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use leveler_client_protocol::{
    ClientCommand, CommandId, NotificationLevel, UiMemoryActionResult, UiMemoryFile,
    UiMemoryFileContent, UiMemoryScope, UiMemoryStatus,
};
use ratatui::Frame;
use ratatui::layout::{Alignment, Constraint, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph, Wrap};
use unicode_width::UnicodeWidthStr;

use crate::action::Effect;
use crate::i18n::MemoryText;
use crate::screen::Screen;
use crate::state::{AppState, Notification};

/// How often the open page re-reads the runtime's counters. Capture and Dream
/// advance asynchronously, so a static page would show a stale queue.
const REFRESH_INTERVAL: Duration = Duration::from_secs(2);

/// Below this width the two panes collapse to one: two 30-column panes are
/// unusable; one full-width pane is not.
const SPLIT_MIN_WIDTH: u16 = 64;

/// The list pane's share of the modal body.
const LIST_WIDTH_RATIO: f64 = 0.40;

/// A second `x` inside this window forgets. Beyond it the first press expires
/// and the next press only arms again.
const CONFIRM_FORGET_WINDOW: Duration = Duration::from_secs(4);

/// The modal never grows wider than this on a wide terminal.
const MODAL_MAX_WIDTH: u16 = 140;
/// The modal never shrinks narrower than this unless the terminal itself is.
const MODAL_MIN_WIDTH: u16 = 60;
/// `MM-DD HH:MM`.
const UPDATED_W: usize = 11;

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

/// The memory overlay's presentation state. Everything projected from the
/// runtime is replaced whole on each answer; nothing here is a second source
/// of truth for the store.
#[derive(Debug, Default)]
pub struct MemoryState {
    pub status: Option<UiMemoryStatus>,
    /// Index into the flattened, filtered entry list (group headers are not
    /// selectable).
    pub selected: usize,
    /// First visible row of the grouped list, counted in rendered rows.
    pub list_scroll: usize,
    pub filter: String,
    pub filter_editing: bool,
    pub preview: Option<UiMemoryFileContent>,
    pub preview_loading: bool,
    /// The selection the current preview was requested for, so an unchanged
    /// selection does not re-read the file every tick.
    preview_for: Option<(String, String)>,
    pub preview_scroll: usize,
    /// Markdown rows built once per (content, width).
    preview_lines: Vec<Line<'static>>,
    preview_lines_generation: u64,
    preview_lines_width: usize,
    preview_lines_theme: Option<crate::theme::ThemeId>,
    preview_generation: u64,
    pub status_query: Option<CommandId>,
    pub preview_query: Option<CommandId>,
    /// Bumped whenever a status refresh invalidates the preview. A read sent
    /// before that bump is stale even when its byte count happens to match.
    preview_epoch: u64,
    preview_query_epoch: u64,
    pub action_query: Option<CommandId>,
    pending_forget: Option<PendingForget>,
    pub notice: Option<String>,
    pub error: Option<String>,
    pub last_refresh: Option<Instant>,
    pub refresh_pending: bool,
    /// Ctrl+F: the same browser fills the terminal.
    pub fullscreen: bool,
    /// Enter: the preview owns the modal body.
    pub expanded: bool,
    /// The selected entry by identity, so a background refresh keeps it.
    selection_key: Option<(String, String)>,
}

/// One selectable file in the unified list. Files are cloned from the
/// runtime's projection so rendering can own its rows while it still publishes
/// hit areas into the page state.
#[derive(Debug, Clone)]
struct Entry {
    scope: MemoryScope,
    file: UiMemoryFile,
}

/// One rendered list row: scope headings and empty/error notices sit between
/// entries but are never selectable.
enum ListRow {
    Group(MemoryScope),
    Empty,
    Error(String),
    Entry { entry: Entry, index: usize },
}

// ---------------------------------------------------------------------------
// Entry points
// ---------------------------------------------------------------------------

/// Open the overlay, or close it when it is already open. The conversation
/// underneath is left untouched: the draft, the running turn and the scroll
/// position all stay where they were.
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
    let previous = state
        .memory
        .selection_key
        .clone()
        .or_else(|| selected_key(state));
    let previous_meta = selected_entry(state)
        .map(|entry| (entry_key(&entry), entry.file.modified_at, entry.file.bytes));
    state.memory.status = Some(*status);
    let (index, key, meta) = {
        let entries = visible_entries(state);
        let found = previous
            .as_ref()
            .and_then(|(scope, path)| {
                entries
                    .iter()
                    .position(|entry| entry.scope.as_str() == scope && entry.file.path == *path)
            })
            .unwrap_or_else(|| default_selection(&entries));
        let key = entries.get(found).map(entry_key);
        let meta = entries
            .get(found)
            .map(|entry| (entry.file.modified_at, entry.file.bytes));
        (found, key, meta)
    };
    state.memory.selected = index;
    state.memory.selection_key = key;
    // A background Capture or Dream can rewrite the file under an open
    // preview. If its identity or stamp moved, drop the cached bytes so the
    // next tick reads the store's current content; the list itself already
    // reflects the new projection.
    let selection_changed = previous_meta
        .as_ref()
        .map(|(previous_key, _, _)| Some(previous_key) != state.memory.selection_key.as_ref())
        .unwrap_or(true);
    let stamp_changed = previous_meta
        .as_ref()
        .map(|(_, modified_at, bytes)| meta != Some((*modified_at, *bytes)))
        .unwrap_or(false);
    if selection_changed || stamp_changed {
        state.memory.preview = None;
        state.memory.preview_for = None;
        state.memory.preview_loading = false;
        state.memory.preview_scroll = 0;
        state.memory.preview_epoch = state.memory.preview_epoch.wrapping_add(1);
    }
}

/// Apply `MemoryFileLoaded`. The page keeps exactly one outstanding preview,
/// so a mismatched id is a stale answer and is dropped. A read for a file the
/// user has already moved away from is also dropped, so fast navigation cannot
/// paint an older file over the current selection.
pub fn apply_file(state: &mut AppState, query_id: Option<&CommandId>, file: UiMemoryFileContent) {
    if query_id.is_none() || state.memory.preview_query.as_ref() != query_id {
        return;
    }
    state.memory.preview_query = None;
    state.memory.preview_loading = false;
    if state.memory.preview_query_epoch != state.memory.preview_epoch {
        state.memory.preview_for = None;
        return;
    }
    if selected_key(state) != Some((file.scope.clone(), file.path.clone())) {
        return;
    }
    // An answer can be read before a background rewrite and arrive after the
    // status that reported it. The listing's byte count is the stamp this
    // projection owns; a read of a different size is stale by definition and
    // must be re-asked instead of painting the old bytes as current.
    let listed_bytes = selected_entry(state).map(|entry| entry.file.bytes);
    if file.error.is_none() && listed_bytes.is_some_and(|bytes| bytes != file.total_bytes) {
        state.memory.preview_for = None;
        return;
    }
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
    state.memory.preview_query_epoch = state.memory.preview_epoch;
    state.memory.preview_loading = true;
    state.memory.preview_for = Some((scope.clone(), path.clone()));
    Some(Effect::Send(ClientCommand::ReadMemoryFile {
        session_id: state.session_id.clone(),
        query_id: Some(query_id),
        scope,
        path,
    }))
}

fn actions_enabled(state: &AppState) -> bool {
    state
        .memory
        .status
        .as_ref()
        .is_some_and(|status| status.enabled)
}

fn dream_effect(state: &mut AppState, batch: bool) -> Vec<Effect> {
    if !actions_enabled(state) {
        state.memory.error = Some(state.t().memory.action_disabled.to_string());
        return Vec::new();
    }
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
        state.memory.error = Some(t.memory.toggle_configured_off.replacen(
            "{}",
            &status.config_path,
            1,
        ));
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
    if !actions_enabled(state) {
        state.memory.error = Some(state.t().memory.action_disabled.to_string());
        return Vec::new();
    }
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
    let Some(entry) = selected_entry(state) else {
        state.memory.error = Some(t.memory.empty.to_string());
        return Vec::new();
    };
    if !entry.file.deletable {
        state.memory.error = Some(t.memory.protected.to_string());
        return Vec::new();
    }
    let scope = entry.scope.as_str().to_owned();
    let path = entry.file.path.clone();
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
    let Some(entry) = selected_entry(state) else {
        return;
    };
    let path = entry.file.path.clone();
    let root = scope_summary(state, entry.scope).map(|summary| summary.root.clone());
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
    let (len, key) = {
        let entries = visible_entries(state);
        let len = entries.len();
        let key = entries.get(index).map(entry_key);
        (len, key)
    };
    if len == 0 {
        state.memory.selected = 0;
        state.memory.selection_key = None;
        return;
    }
    let next = index.min(len - 1);
    let previous = state.memory.selection_key.clone();
    state.memory.selected = next;
    state.memory.selection_key = key;
    if previous != state.memory.selection_key {
        state.memory.preview = None;
        state.memory.preview_for = None;
        state.memory.preview_loading = false;
        state.memory.preview_scroll = 0;
    }
}

fn move_selection(state: &mut AppState, delta: isize) {
    let len = visible_len(state);
    if len == 0 {
        state.memory.selected = 0;
        state.memory.selection_key = None;
        return;
    }
    let current = state.memory.selected as isize;
    set_selection(state, (current + delta).clamp(0, len as isize - 1) as usize);
}

fn clamp_selection(state: &mut AppState) {
    let (len, key) = {
        let entries = visible_entries(state);
        let len = entries.len();
        let key = entries.get(state.memory.selected).map(entry_key);
        (len, key)
    };
    if len == 0 {
        state.memory.selected = 0;
        state.memory.selection_key = None;
    } else {
        if state.memory.selected >= len {
            state.memory.selected = len - 1;
        }
        state.memory.selection_key = key;
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

/// Ctrl+F fills the terminal with the same browser; every bit of state — the
/// selection, the filter, both scroll positions — stays in this struct.
fn toggle_fullscreen(state: &mut AppState) {
    state.memory.fullscreen = !state.memory.fullscreen;
}

fn handle_filter_key(state: &mut AppState, key: &KeyEvent) -> Vec<Effect> {
    match key.code {
        KeyCode::Esc | KeyCode::Enter => {
            state.memory.filter_editing = false;
        }
        KeyCode::Backspace => {
            state.memory.filter.pop();
            state.memory.selected = 0;
            state.memory.list_scroll = 0;
            clamp_selection(state);
        }
        KeyCode::Char('u')
            if key
                .modifiers
                .contains(crossterm::event::KeyModifiers::CONTROL) =>
        {
            state.memory.filter.clear();
            state.memory.selected = 0;
            state.memory.list_scroll = 0;
            clamp_selection(state);
        }
        KeyCode::Char(character)
            if !key
                .modifiers
                .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
        {
            state.memory.filter.push(character);
            state.memory.selected = 0;
            state.memory.list_scroll = 0;
            clamp_selection(state);
        }
        _ => {}
    }
    Vec::new()
}

fn handle_list_key(state: &mut AppState, key: &KeyEvent) -> Vec<Effect> {
    match key.code {
        KeyCode::Esc => {
            back(state);
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
        KeyCode::Char('/') => {
            state.memory.filter_editing = true;
            Vec::new()
        }
        KeyCode::Enter => {
            if selected_entry(state).is_some() {
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

/// Narrow/expanded layout: the preview owns the body, so the list's keys
/// scroll it.
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

/// One level back: search → reading → fullscreen → conversation. The same rule
/// backs Esc and the shared back navigation.
pub fn back(state: &mut AppState) {
    if state.memory.filter_editing {
        state.memory.filter_editing = false;
        return;
    }
    if state.memory.expanded {
        state.memory.expanded = false;
        state.memory.preview_scroll = 0;
        return;
    }
    if state.memory.fullscreen {
        state.memory.fullscreen = false;
        return;
    }
    close(state);
}

// ---------------------------------------------------------------------------
// Mouse
// ---------------------------------------------------------------------------

/// A wheel notch moves the list cursor; in the reading mode it scrolls the
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

/// A click on a painted list row, keyed by its index in the flattened entry
/// list.
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

fn scope_summary(state: &AppState, scope: MemoryScope) -> Option<&UiMemoryScope> {
    let status = state.memory.status.as_ref()?;
    Some(match scope {
        MemoryScope::Global => &status.global,
        MemoryScope::Workspace => &status.workspace,
    })
}

fn matches_filter(file: &UiMemoryFile, filter: &str) -> bool {
    if filter.is_empty() {
        return true;
    }
    let needle = filter.to_lowercase();
    file.path.to_lowercase().contains(&needle)
        || file.kind.to_lowercase().contains(&needle)
        || display_name(file).to_lowercase().contains(&needle)
}

/// Manifest first, then topics, then observations; path order inside a kind.
fn sort_key(file: &UiMemoryFile) -> (u8, &str) {
    let kind = match file.kind.as_str() {
        "manifest" => 0,
        "topic" => 1,
        "observation" => 2,
        "archive" => 3,
        _ => 4,
    };
    (kind, file.path.as_str())
}

fn display_name(file: &UiMemoryFile) -> String {
    display_path_name(&file.path)
}

fn display_path_name(path: &str) -> String {
    if path == "MEMORY.md" {
        return path.to_owned();
    }
    let name = path.rsplit('/').next().unwrap_or(path);
    name.strip_suffix(".md").unwrap_or(name).to_owned()
}

fn entry_key(entry: &Entry) -> (String, String) {
    (entry.scope.as_str().to_owned(), entry.file.path.clone())
}

/// Every filtered entry, grouped Global then Workspace, in one flat list.
fn visible_entries(state: &AppState) -> Vec<Entry> {
    let Some(status) = state.memory.status.as_ref() else {
        return Vec::new();
    };
    let mut entries = Vec::new();
    for scope in [MemoryScope::Global, MemoryScope::Workspace] {
        let summary = match scope {
            MemoryScope::Global => &status.global,
            MemoryScope::Workspace => &status.workspace,
        };
        if summary.error.is_some() {
            continue;
        }
        let mut files: Vec<&UiMemoryFile> = summary
            .files
            .iter()
            .filter(|file| matches_filter(file, &state.memory.filter))
            .collect();
        files.sort_by(|left, right| sort_key(left).cmp(&sort_key(right)));
        entries.extend(files.into_iter().map(|file| Entry {
            scope,
            file: file.clone(),
        }));
    }
    entries
}

fn visible_len(state: &AppState) -> usize {
    visible_entries(state).len()
}

fn selected_entry(state: &AppState) -> Option<Entry> {
    visible_entries(state)
        .into_iter()
        .nth(state.memory.selected)
}

fn selected_key(state: &AppState) -> Option<(String, String)> {
    selected_entry(state).map(|entry| entry_key(&entry))
}

/// The default the product spec pins: the workspace index when it exists,
/// otherwise the first workspace note, then the global side.
fn default_selection(entries: &[Entry]) -> usize {
    entries
        .iter()
        .position(|entry| entry.scope == MemoryScope::Workspace && entry.file.path == "MEMORY.md")
        .or_else(|| {
            entries
                .iter()
                .position(|entry| entry.scope == MemoryScope::Workspace)
        })
        .or_else(|| {
            entries.iter().position(|entry| {
                entry.scope == MemoryScope::Global && entry.file.path == "MEMORY.md"
            })
        })
        .unwrap_or(0)
}

// ---------------------------------------------------------------------------
// Rendering
// ---------------------------------------------------------------------------

/// Draw the centered memory overlay over the conversation. The caller renders
/// the workbench first, so the transcript, composer draft and background turn
/// remain visible around the modal.
pub fn render_overlay(frame: &mut Frame, area: Rect, state: &mut AppState) {
    state.screen_rows.clear();
    if area.width < 20 || area.height < 8 {
        return;
    }
    let rect = modal_rect(area, state.memory.fullscreen);
    let theme = state.theme.clone();
    let t = state.t();
    let title = if state
        .memory
        .status
        .as_ref()
        .is_some_and(|status| status.rollout != "off" && !status.enabled)
    {
        format!("{} · {}", t.memory.title, t.memory.paused_label)
    } else {
        t.memory.title.to_owned()
    };
    frame.render_widget(Clear, rect);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(theme.border.normal))
        .title(Span::styled(
            format!(" {title} "),
            Style::default()
                .fg(theme.accent.primary)
                .add_modifier(Modifier::BOLD),
        ));
    frame.render_widget(&block, rect);
    let inner = block.inner(rect);
    if inner.width < 4 || inner.height < 4 {
        return;
    }
    let footer_rows =
        footer_rows(state, inner.width as usize).min(inner.height.saturating_sub(2).max(1));
    let chunks =
        Layout::vertical([Constraint::Min(1), Constraint::Length(footer_rows)]).split(inner);
    let body = chunks[0];
    let footer = chunks[1];
    let (first_hint, second_hint) = footer_text(state);
    let (body, status_row) = if state.memory.error.is_some() || state.memory.notice.is_some() {
        let parts = Layout::vertical([Constraint::Min(1), Constraint::Length(1)]).split(body);
        (parts[0], Some(parts[1]))
    } else {
        (body, None)
    };

    let status = state.memory.status.clone();
    if status
        .as_ref()
        .is_some_and(|status| status.rollout == "off")
    {
        render_off(frame, body, state, status.as_ref());
        render_memory_status(frame, status_row, state);
        render_footer(frame, footer, state, &first_hint, &second_hint);
        return;
    }
    if status.is_none() {
        render_loading(frame, body, state);
        render_memory_status(frame, status_row, state);
        render_footer(frame, footer, state, &first_hint, &second_hint);
        return;
    }
    if state.memory.expanded || body.width < SPLIT_MIN_WIDTH {
        render_preview(frame, body, state);
        render_memory_status(frame, status_row, state);
        render_footer(frame, footer, state, &first_hint, &second_hint);
        return;
    }
    let list_width = ((body.width as f64) * LIST_WIDTH_RATIO) as u16;
    let list_width = list_width.clamp(20, body.width.saturating_sub(12));
    if body.width.saturating_sub(list_width) > 2 {
        let panes = Layout::horizontal([
            Constraint::Length(list_width),
            Constraint::Length(1),
            Constraint::Min(1),
        ])
        .split(body);
        render_list(frame, panes[0], state);
        render_separator(frame, panes[1], state);
        render_preview(frame, panes[2], state);
    } else {
        render_list(frame, body, state);
    }
    if let Some((x, y)) = search_cursor(state, body) {
        frame.set_cursor_position(ratatui::layout::Position::new(x, y));
    }
    render_memory_status(frame, status_row, state);
    render_footer(frame, footer, state, &first_hint, &second_hint);
}

/// The page's own outcome line: an action's pending/success text or a real
/// error, always visible instead of being appended below the list rows.
fn render_memory_status(frame: &mut Frame, area: Option<Rect>, state: &AppState) {
    let Some(area) = area else {
        return;
    };
    let theme = &state.theme;
    let t = state.t();
    let (text, color) = if let Some(error) = state.memory.error.as_ref() {
        (
            t.memory.error_label.replacen("{}", error, 1),
            theme.status.error,
        )
    } else if let Some(notice) = state.memory.notice.as_ref() {
        (notice.clone(), theme.status.success)
    } else {
        return;
    };
    let width = area.width.saturating_sub(2) as usize;
    frame.render_widget(
        Paragraph::new(crate::render::truncate_display(&text, width))
            .style(Style::default().fg(color)),
        inset(area, 1, 0),
    );
}

fn modal_rect(area: Rect, fullscreen: bool) -> Rect {
    if fullscreen {
        return area;
    }
    let margin = ((area.height as f32) * 0.06).round() as u16;
    let margin = margin.clamp(1, 3).min(area.height / 4);
    let height = area.height.saturating_sub(margin * 2).max(6);
    let max_width = area.width.saturating_sub(4).min(MODAL_MAX_WIDTH);
    let preferred = ((area.width as f32) * 0.85) as u16;
    let width = preferred
        .min(max_width)
        .max(MODAL_MIN_WIDTH)
        .min(area.width);
    Rect {
        x: area.x + (area.width.saturating_sub(width)) / 2,
        y: area.y + (area.height.saturating_sub(height)) / 2,
        width,
        height,
    }
}

fn render_off(frame: &mut Frame, area: Rect, state: &AppState, status: Option<&UiMemoryStatus>) {
    let theme = &state.theme;
    let t = state.t();
    let path = status
        .map(|status| status.config_path.as_str())
        .filter(|path| !path.is_empty())
        .unwrap_or("config.toml");
    let text = t.memory.off.replacen("{}", path, 1);
    let block = Paragraph::new(text)
        .wrap(Wrap { trim: false })
        .style(Style::default().fg(theme.text.primary));
    let inner = inset(area, 2, 1);
    frame.render_widget(block, inner);
}

fn render_loading(frame: &mut Frame, area: Rect, state: &AppState) {
    let theme = &state.theme;
    let t = state.t();
    frame.render_widget(
        Paragraph::new(t.memory.loading).style(Style::default().fg(theme.text.muted)),
        inset(area, 1, 0),
    );
}

fn search_line(state: &AppState, width: usize) -> Line<'static> {
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

fn search_cursor(state: &AppState, body: Rect) -> Option<(u16, u16)> {
    if !state.memory.filter_editing {
        return None;
    }
    let t = state.t();
    let prefix = t.memory.filter_label.split("{}").next().unwrap_or_default();
    let x = body
        .x
        .saturating_add(UnicodeWidthStr::width(prefix) as u16)
        .saturating_add(UnicodeWidthStr::width(state.memory.filter.as_str()) as u16);
    (x < body.x.saturating_add(body.width)).then_some((x, body.y))
}

fn render_list(frame: &mut Frame, area: Rect, state: &mut AppState) {
    if area.width == 0 || area.height == 0 {
        return;
    }
    let width = area.width as usize;
    let theme = state.theme.clone();
    let t = state.t();
    let entries = visible_entries(state);
    let rows = list_rows(state, &entries);
    let selected_row = rows.iter().position(
        |row| matches!(row, ListRow::Entry { index, .. } if *index == state.memory.selected),
    );
    let visible_rows = (area.height as usize).saturating_sub(1);
    if let Some(row) = selected_row {
        if row < state.memory.list_scroll {
            state.memory.list_scroll = row;
        } else if visible_rows > 0 && row >= state.memory.list_scroll + visible_rows {
            state.memory.list_scroll = row + 1 - visible_rows;
        }
    }
    let mut lines: Vec<Line<'static>> = Vec::new();
    let mut hits: Vec<(Rect, String)> = Vec::new();
    lines.push(search_line(state, width));
    if entries.is_empty()
        && !state.memory.filter.is_empty()
        && !rows.iter().any(|row| matches!(row, ListRow::Error(_)))
    {
        push_wrapped(&mut lines, t.memory.empty_filter, theme.text.muted, width);
    } else {
        for row in rows.iter().skip(state.memory.list_scroll) {
            if lines.len() > visible_rows {
                break;
            }
            match row {
                ListRow::Group(scope) => lines.push(Line::from(Span::styled(
                    scope.label(&t.memory),
                    Style::default()
                        .fg(theme.text.muted)
                        .add_modifier(Modifier::BOLD),
                ))),
                ListRow::Empty => lines.push(Line::from(Span::styled(
                    format!("  {}", t.memory.scope_empty),
                    Style::default().fg(theme.text.muted),
                ))),
                ListRow::Error(message) => {
                    lines.push(Line::from(Span::styled(
                        format!("  {}", t.memory.error_label.replacen("{}", message, 1)),
                        Style::default().fg(theme.status.error),
                    )));
                }
                ListRow::Entry { entry, index } => {
                    let selected = *index == state.memory.selected;
                    let y = area.y + lines.len() as u16;
                    if y < area.y + area.height {
                        hits.push((Rect::new(area.x, y, area.width, 1), index.to_string()));
                    }
                    lines.push(entry_row(entry, selected, width, &theme));
                }
            }
        }
    }
    state.screen_rows = hits;
    frame.render_widget(Paragraph::new(lines), area);
}

/// Build the rendered row sequence: a group heading, its empty/error notice,
/// or its entries. An empty group still shows its heading so the two scopes
/// are always visible, even on a fresh install.
fn list_rows(state: &AppState, entries: &[Entry]) -> Vec<ListRow> {
    let mut rows = Vec::new();
    let mut index = 0;
    for scope in [MemoryScope::Global, MemoryScope::Workspace] {
        let group: Vec<Entry> = entries
            .iter()
            .filter(|entry| entry.scope == scope)
            .cloned()
            .collect();
        let error = scope_summary(state, scope).and_then(|summary| summary.error.clone());
        if let Some(error) = error {
            rows.push(ListRow::Group(scope));
            rows.push(ListRow::Error(error));
            continue;
        }
        if group.is_empty() && !state.memory.filter.is_empty() {
            continue;
        }
        rows.push(ListRow::Group(scope));
        if group.is_empty() {
            rows.push(ListRow::Empty);
            continue;
        }
        for entry in group {
            rows.push(ListRow::Entry { entry, index });
            index += 1;
        }
    }
    rows
}

fn render_separator(frame: &mut Frame, area: Rect, state: &AppState) {
    let theme = &state.theme;
    let line = "│";
    let lines: Vec<Line<'static>> = (0..area.height)
        .map(|_| Line::from(Span::styled(line, Style::default().fg(theme.border.subtle))))
        .collect();
    frame.render_widget(Paragraph::new(lines), area);
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
        let empty_active = state
            .memory
            .status
            .as_ref()
            .is_some_and(|status| status.enabled)
            && visible_entries(state).is_empty();
        let text = if empty_active {
            let status = state.memory.status.as_ref().expect("checked above");
            let mut text = t.memory.empty_enabled.to_owned();
            if status.capture_enabled {
                text.push_str("\n\n");
                text.push_str(t.memory.empty_capture);
            }
            if status.automatic_dream_enabled {
                text.push('\n');
                text.push_str(t.memory.empty_dream);
            }
            text
        } else if state.memory.preview_loading {
            t.memory.preview_loading.to_owned()
        } else {
            t.memory.no_preview.to_owned()
        };
        frame.render_widget(
            Paragraph::new(text)
                .wrap(Wrap { trim: false })
                .style(Style::default().fg(theme.text.muted)),
            inset(area, 1, 0),
        );
        return;
    };
    let title = format!("{} · {}", display_path_name(&file.path), t.memory.read_only);
    lines.push(Line::from(Span::styled(
        crate::render::truncate_display(&title, width),
        Style::default()
            .fg(theme.accent.primary)
            .add_modifier(Modifier::BOLD),
    )));
    lines.push(Line::from(Span::styled(
        "─".repeat(width),
        Style::default().fg(theme.border.subtle),
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
        if state.memory.preview_lines_generation != generation
            || state.memory.preview_lines_width != width
            || state.memory.preview_lines_theme != Some(theme.id)
        {
            state.memory.preview_lines =
                crate::markdown::MdDoc::parse(&file.content).to_lines(width, &theme);
            state.memory.preview_lines_generation = generation;
            state.memory.preview_lines_width = width;
            state.memory.preview_lines_theme = Some(theme.id);
        }
        let available = (area.height as usize).saturating_sub(lines.len());
        let total = state.memory.preview_lines.len();
        let max_scroll = total.saturating_sub(available);
        if state.memory.preview_scroll > max_scroll {
            state.memory.preview_scroll = max_scroll;
        }
        lines.extend(
            state
                .memory
                .preview_lines
                .iter()
                .skip(state.memory.preview_scroll)
                .take(available)
                .cloned(),
        );
    }
    frame.render_widget(Paragraph::new(lines), area);
}

/// The two footer lines for the current mode, before width-aware wrapping.
fn footer_text(state: &AppState) -> (String, String) {
    let t = state.t();
    let status = state.memory.status.as_ref();
    if state.memory.filter_editing {
        (t.memory.footer_filter.to_string(), String::new())
    } else if status.is_some_and(|status| status.rollout == "off") {
        (t.memory.footer_off.to_string(), String::new())
    } else if state.memory.expanded {
        (t.memory.footer_preview.to_string(), String::new())
    } else {
        let paused = status.is_some_and(|status| !status.enabled);
        (
            if paused {
                t.memory.footer_list_resume
            } else {
                t.memory.footer_list
            }
            .to_string(),
            if paused {
                t.memory.footer_manage_paused
            } else {
                t.memory.footer_manage
            }
            .to_string(),
        )
    }
}

/// Reserve as many rows as the hints need at this width, so a narrow terminal
/// wraps them instead of clipping the tail (the reference layout wraps too).
fn footer_rows(state: &AppState, width: usize) -> u16 {
    let (first, second) = footer_text(state);
    let rows = wrapped_rows(&first, width) + wrapped_rows(&second, width);
    rows.clamp(1, 4) as u16
}

fn wrapped_rows(text: &str, width: usize) -> usize {
    if text.is_empty() || width == 0 {
        return 0;
    }
    crate::render::wrap(text, width).len().max(1)
}

fn render_footer(frame: &mut Frame, area: Rect, state: &AppState, first: &str, second: &str) {
    let theme = &state.theme;
    let mut lines = vec![Line::from(Span::styled(
        first.to_owned(),
        Style::default().fg(theme.text.muted),
    ))];
    if !second.is_empty() {
        lines.push(Line::from(Span::styled(
            second.to_owned(),
            Style::default().fg(theme.text.subtle),
        )));
    }
    frame.render_widget(
        Paragraph::new(lines)
            .wrap(Wrap { trim: true })
            .alignment(Alignment::Center),
        area,
    );
}

fn entry_row(
    entry: &Entry,
    selected: bool,
    width: usize,
    theme: &crate::theme::Theme,
) -> Line<'static> {
    let marker = if selected { "› " } else { "  " };
    let time = updated_label(entry.file.modified_at);
    let budget = width.saturating_sub(2 + 1 + UPDATED_W).max(1);
    let name = crate::render::truncate_display(&display_name(&entry.file), budget);
    let padded = crate::render::pad_display(&name, budget);
    let base = if selected {
        Style::default()
            .fg(theme.text.primary)
            .bg(theme.surface.selection)
    } else {
        Style::default().fg(theme.text.primary)
    };
    let time_style = if selected {
        base
    } else {
        Style::default().fg(theme.text.muted)
    };
    Line::from(vec![
        Span::styled(marker.to_owned(), base),
        Span::styled(padded, base),
        Span::styled(format!(" {time:>UPDATED_W$}"), time_style),
    ])
}

/// `MM-DD HH:MM` in local time.
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

fn inset(area: Rect, x: u16, y: u16) -> Rect {
    Rect {
        x: area.x.saturating_add(x),
        y: area.y.saturating_add(y),
        width: area.width.saturating_sub(x * 2),
        height: area.height.saturating_sub(y * 2),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::i18n::Locale;
    use crate::state::Boot;
    use crate::theme::Theme;
    use leveler_client_protocol::{SessionId, UiMemoryActivity, UiMemorySession};
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

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

    fn file(scope: &str, path: &str, kind: &str, modified_at: Option<i64>) -> UiMemoryFile {
        UiMemoryFile {
            scope: scope.to_owned(),
            path: path.to_owned(),
            kind: kind.to_owned(),
            bytes: 10,
            modified_at,
            deletable: matches!(kind, "topic" | "observation"),
        }
    }

    fn scope(scope: &str, root: &str, files: Vec<UiMemoryFile>) -> UiMemoryScope {
        UiMemoryScope {
            scope: scope.to_owned(),
            root: root.to_owned(),
            files,
            ..Default::default()
        }
    }

    fn memory_status() -> UiMemoryStatus {
        UiMemoryStatus {
            rollout: "active".to_owned(),
            enabled: true,
            config_path: "/home/u/.leveler/config.toml".to_owned(),
            capture_enabled: true,
            automatic_dream_enabled: true,
            manual_dream_enabled: true,
            batch_dream_enabled: true,
            workspace_ephemeral: false,
            global: scope(
                "global",
                "/mem/global",
                vec![
                    file("global", "MEMORY.md", "manifest", None),
                    file("global", "topics/editor.md", "topic", Some(1_700_000_000)),
                ],
            ),
            workspace: scope(
                "workspace",
                "/mem/workspace",
                vec![
                    file("workspace", "MEMORY.md", "manifest", None),
                    file("workspace", "topics/build.md", "topic", Some(1_700_000_000)),
                    file("workspace", "topics/cache.md", "topic", Some(1_700_000_000)),
                    file(
                        "workspace",
                        "topics/工程约定.md",
                        "topic",
                        Some(1_700_000_000),
                    ),
                ],
            ),
            session: Some(UiMemorySession::default()),
            activity: UiMemoryActivity::default(),
        }
    }

    fn install_status(state: &mut AppState, status: UiMemoryStatus) {
        let query_id = CommandId::generate();
        state.memory.status_query = Some(query_id.clone());
        apply_status(state, Some(&query_id), Box::new(status));
    }

    fn state_with_status() -> AppState {
        let mut state = state();
        install_status(&mut state, memory_status());
        state
    }

    fn select(state: &mut AppState, scope: MemoryScope, path: &str) {
        let index = visible_entries(state)
            .iter()
            .position(|entry| entry.scope == scope && entry.file.path == path)
            .expect("entry exists");
        set_selection(state, index);
    }

    fn entry(state: &AppState, scope: MemoryScope, path: &str) -> Entry {
        visible_entries(state)
            .into_iter()
            .find(|entry| entry.scope == scope && entry.file.path == path)
            .expect("entry exists")
    }

    fn rendered(state: &mut AppState, width: u16, height: u16) -> String {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal
            .draw(|frame| render_overlay(frame, frame.area(), state))
            .unwrap();
        let buffer = terminal.backend().buffer();
        (0..buffer.area.height)
            .map(|y| {
                let mut line = String::new();
                let mut skip = false;
                for x in 0..buffer.area.width {
                    if skip {
                        skip = false;
                        continue;
                    }
                    let symbol = buffer[(x, y)].symbol();
                    line.push_str(symbol);
                    if UnicodeWidthStr::width(symbol) >= 2 {
                        // A wide symbol owns the next cell as its continuation.
                        skip = true;
                    }
                }
                line
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn the_default_selection_is_the_workspace_index() {
        let state = state_with_status();
        let selected = selected_entry(&state).expect("a default selection");
        assert_eq!(selected.scope, MemoryScope::Workspace);
        assert_eq!(selected.file.path, "MEMORY.md");
    }

    #[test]
    fn both_scopes_share_one_list_without_a_tab_switch() {
        let state = state_with_status();
        let entries = visible_entries(&state);
        assert!(
            entries
                .iter()
                .any(|entry| entry.scope == MemoryScope::Global)
        );
        assert!(
            entries
                .iter()
                .any(|entry| entry.scope == MemoryScope::Workspace)
        );
        assert_eq!(entries.len(), 6);
        let rows = list_rows(&state, &entries);
        assert_eq!(
            rows.iter()
                .filter(|row| matches!(row, ListRow::Group(_)))
                .count(),
            2
        );
    }

    #[test]
    fn the_search_covers_both_scopes_and_keeps_the_selection_valid() {
        let mut state = state_with_status();
        state.memory.filter = "editor".to_owned();
        clamp_selection(&mut state);
        assert_eq!(visible_len(&state), 1);
        assert_eq!(
            selected_entry(&state).map(|entry| entry.scope),
            Some(MemoryScope::Global)
        );

        state.memory.filter = "cache".to_owned();
        clamp_selection(&mut state);
        assert_eq!(
            selected_entry(&state).map(|entry| entry.file.path.clone()),
            Some("topics/cache.md".to_owned())
        );

        state.memory.filter = "nothing-matches".to_owned();
        clamp_selection(&mut state);
        assert_eq!(visible_len(&state), 0);
        assert!(selected_entry(&state).is_none());
    }

    #[test]
    fn a_background_rewrite_of_the_open_file_invalidates_the_preview() {
        let mut state = state_with_status();
        select(&mut state, MemoryScope::Workspace, "topics/build.md");
        state.memory.preview = Some(UiMemoryFileContent {
            scope: "workspace".to_owned(),
            path: "topics/build.md".to_owned(),
            content: "before capture".to_owned(),
            content_hash: "d".repeat(64),
            truncated: false,
            total_bytes: 14,
            error: None,
        });
        state.memory.preview_for = Some(("workspace".to_owned(), "topics/build.md".to_owned()));
        state.memory.preview_loading = false;

        let mut updated = memory_status();
        for file in &mut updated.workspace.files {
            if file.path == "topics/build.md" {
                file.modified_at = Some(1_700_000_500);
            }
        }
        install_status(&mut state, updated);
        assert_eq!(
            selected_key(&state),
            Some(("workspace".to_owned(), "topics/build.md".to_owned())),
            "the reader keeps its selection"
        );
        assert!(state.memory.preview.is_none());
        assert!(state.memory.preview_for.is_none());
    }

    #[test]
    fn a_preview_read_from_before_a_background_rewrite_is_discarded() {
        let mut state = state_with_status();
        select(&mut state, MemoryScope::Workspace, "topics/build.md");
        let query_id = CommandId::generate();
        state.memory.preview_query = Some(query_id.clone());
        apply_file(
            &mut state,
            Some(&query_id),
            UiMemoryFileContent {
                scope: "workspace".to_owned(),
                path: "topics/build.md".to_owned(),
                content: "old bytes".to_owned(),
                content_hash: "e".repeat(64),
                truncated: false,
                total_bytes: 9,
                error: None,
            },
        );
        assert!(
            state.memory.preview.is_none(),
            "a read that predates the listed stamp is not current"
        );
        assert!(state.memory.preview_for.is_none());

        let fresh = CommandId::generate();
        state.memory.preview_query = Some(fresh.clone());
        state.memory.preview_query_epoch = state.memory.preview_epoch;
        apply_file(
            &mut state,
            Some(&fresh),
            UiMemoryFileContent {
                scope: "workspace".to_owned(),
                path: "topics/build.md".to_owned(),
                content: "new bytes".to_owned(),
                content_hash: "f".repeat(64),
                truncated: false,
                total_bytes: 10,
                error: None,
            },
        );
        assert!(
            state.memory.preview.is_some(),
            "a read matching the listing is applied"
        );
    }

    #[test]
    fn a_preview_requested_before_a_refresh_bump_is_dropped_even_at_equal_size() {
        let mut state = state_with_status();
        select(&mut state, MemoryScope::Workspace, "topics/build.md");
        let query_id = CommandId::generate();
        state.memory.preview_query = Some(query_id.clone());
        state.memory.preview_query_epoch = 0;
        state.memory.preview_epoch = 1;
        apply_file(
            &mut state,
            Some(&query_id),
            UiMemoryFileContent {
                scope: "workspace".to_owned(),
                path: "topics/build.md".to_owned(),
                content: "same size".to_owned(),
                content_hash: "a".repeat(64),
                truncated: false,
                total_bytes: 10,
                error: None,
            },
        );
        assert!(state.memory.preview.is_none());
        assert!(state.memory.preview_for.is_none());
    }

    #[test]
    fn a_background_refresh_keeps_the_selected_entry() {
        let mut state = state_with_status();
        select(&mut state, MemoryScope::Workspace, "topics/build.md");
        install_status(&mut state, memory_status());
        let selected = selected_entry(&state).expect("selection survived");
        assert_eq!(selected.scope, MemoryScope::Workspace);
        assert_eq!(selected.file.path, "topics/build.md");
    }

    #[test]
    fn a_stale_preview_answer_is_dropped_after_the_selection_moves() {
        let mut state = state_with_status();
        select(&mut state, MemoryScope::Workspace, "topics/build.md");
        let query_id = CommandId::generate();
        state.memory.preview_query = Some(query_id.clone());
        state.memory.preview_loading = true;
        select(&mut state, MemoryScope::Workspace, "topics/cache.md");
        apply_file(
            &mut state,
            Some(&query_id),
            UiMemoryFileContent {
                scope: "workspace".to_owned(),
                path: "topics/build.md".to_owned(),
                content: "old".to_owned(),
                content_hash: "a".repeat(64),
                truncated: false,
                total_bytes: 3,
                error: None,
            },
        );
        assert!(state.memory.preview.is_none());
        assert!(state.memory.preview_query.is_none());
        assert_eq!(
            selected_key(&state),
            Some(("workspace".to_owned(), "topics/cache.md".to_owned()))
        );
    }

    #[test]
    fn ctrl_f_fills_the_terminal_without_running_flush() {
        let mut state = state_with_status();
        let key = KeyEvent::new(KeyCode::Char('f'), KeyModifiers::CONTROL);
        handle_key(&mut state, &key);
        assert!(state.memory.fullscreen);
        assert!(state.memory.action_query.is_none());
        assert_eq!(state.active_screen, Screen::Conversation);
        select(&mut state, MemoryScope::Workspace, "topics/cache.md");
        state.memory.filter = "cache".to_owned();
        clamp_selection(&mut state);
        handle_key(&mut state, &key);
        assert!(!state.memory.fullscreen);
        assert_eq!(state.memory.filter, "cache");
        assert_eq!(
            selected_entry(&state).map(|entry| entry.file.path.clone()),
            Some("topics/cache.md".to_owned())
        );
    }

    #[test]
    fn esc_unwinds_search_then_reader_then_fullscreen_then_the_overlay() {
        let mut state = state_with_status();
        state.active_screen = Screen::Memory;
        let esc = KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE);
        state.memory.filter_editing = true;
        handle_key(&mut state, &esc);
        assert!(!state.memory.filter_editing);
        assert_eq!(state.active_screen, Screen::Memory);

        state.memory.expanded = true;
        state.memory.fullscreen = true;
        handle_key(&mut state, &esc);
        assert!(!state.memory.expanded);
        assert_eq!(state.active_screen, Screen::Memory);

        handle_key(&mut state, &esc);
        assert!(!state.memory.fullscreen);
        assert_eq!(state.active_screen, Screen::Memory);

        handle_key(&mut state, &esc);
        assert_eq!(state.active_screen, Screen::Conversation);
    }

    #[test]
    fn enter_reads_in_place_instead_of_opening_a_screen() {
        let mut state = state_with_status();
        state.active_screen = Screen::Memory;
        handle_key(
            &mut state,
            &KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
        );
        assert!(state.memory.expanded);
        assert_eq!(state.active_screen, Screen::Memory);
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
        install_status(&mut state, paused);
        let effects = handle_key(
            &mut state,
            &KeyEvent::new(KeyCode::Char('t'), KeyModifiers::NONE),
        );
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
        off.config_path = "/tmp/real-home/config.toml".to_owned();
        install_status(&mut state, off);
        let effects = handle_key(
            &mut state,
            &KeyEvent::new(KeyCode::Char('t'), KeyModifiers::NONE),
        );
        assert!(
            effects.is_empty(),
            "an off configuration cannot be promoted by the page"
        );
        let error = state.memory.error.expect("an explanation is shown");
        assert!(error.contains("/tmp/real-home/config.toml"), "{error}");
        assert!(state.memory.action_query.is_none());
    }

    #[test]
    fn capture_and_dream_are_refused_while_memory_is_not_enabled() {
        let mut state = state_with_status();
        let mut paused = memory_status();
        paused.enabled = false;
        install_status(&mut state, paused);
        for key in ['d', 'D', 'f'] {
            let effects = handle_key(
                &mut state,
                &KeyEvent::new(KeyCode::Char(key), KeyModifiers::NONE),
            );
            assert!(effects.is_empty(), "{key} must not send an action");
            assert!(state.memory.error.is_some());
        }
    }

    #[test]
    fn the_first_x_arms_and_a_second_x_inside_the_window_sends_forget() {
        let mut state = state_with_status();
        select(&mut state, MemoryScope::Workspace, "topics/build.md");
        state.memory.preview = Some(UiMemoryFileContent {
            scope: "workspace".to_owned(),
            path: "topics/build.md".to_owned(),
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
        let Effect::Send(ClientCommand::ForgetMemory { scope, path, .. }) = &second[0] else {
            panic!("second x must forget, got {second:?}");
        };
        assert_eq!(scope, "workspace");
        assert_eq!(path, "topics/build.md");
    }

    #[test]
    fn a_protected_manifest_cannot_be_forgotten() {
        let mut state = state_with_status();
        select(&mut state, MemoryScope::Workspace, "MEMORY.md");
        let effects = press_forget(&mut state);
        assert!(effects.is_empty());
        assert!(state.memory.error.is_some());
        assert!(state.memory.pending_forget.is_none());
    }

    #[test]
    fn a_truncated_preview_is_not_a_forget_hash() {
        let mut state = state_with_status();
        select(&mut state, MemoryScope::Workspace, "topics/build.md");
        state.memory.preview = Some(UiMemoryFileContent {
            scope: "workspace".to_owned(),
            path: "topics/build.md".to_owned(),
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
    fn a_successful_management_action_rereads_the_preview() {
        let mut state = state_with_status();
        state.active_screen = Screen::Memory;
        select(&mut state, MemoryScope::Workspace, "topics/build.md");
        state.memory.preview = Some(UiMemoryFileContent {
            scope: "workspace".to_owned(),
            path: "topics/build.md".to_owned(),
            content: "stale".to_owned(),
            content_hash: "b".repeat(64),
            truncated: false,
            total_bytes: 5,
            error: None,
        });
        state.memory.preview_for = Some(("workspace".to_owned(), "topics/build.md".to_owned()));
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
        assert!(state.memory.preview.is_none());
        assert!(state.memory.preview_for.is_none());
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

    #[test]
    fn a_file_row_right_aligns_the_update_time_and_fits_the_width() {
        let topic = entry(
            &state_with_status(),
            MemoryScope::Workspace,
            "topics/build.md",
        );
        let theme = Theme::no_color();
        let expected = chrono::DateTime::from_timestamp(1_700_000_000, 0)
            .unwrap()
            .with_timezone(&chrono::Local)
            .format("%m-%d %H:%M")
            .to_string();
        for width in [24, 32, 40, 60] {
            let row = entry_row(&topic, false, width, &theme);
            let text: String = row.spans.iter().map(|span| span.content.as_ref()).collect();
            assert!(
                unicode_width::UnicodeWidthStr::width(text.as_str()) <= width,
                "row wider than {width}: {text:?}"
            );
            assert!(text.ends_with(&expected), "{text:?}");
        }
    }

    #[test]
    fn a_file_row_without_a_modified_time_shows_a_placeholder() {
        let mut state = state_with_status();
        state.memory.status = Some({
            let mut status = memory_status();
            status
                .workspace
                .files
                .push(file("workspace", "topics/no-time.md", "topic", None));
            status
        });
        let topic = entry(&state, MemoryScope::Workspace, "topics/no-time.md");
        let row = entry_row(&topic, false, 80, &Theme::no_color());
        let text: String = row.spans.iter().map(|span| span.content.as_ref()).collect();
        assert!(
            text.trim_end().ends_with('-'),
            "the time column keeps its width with no time: {text:?}"
        );
    }

    #[test]
    fn a_cjk_title_is_truncated_by_display_width_before_the_time_column() {
        let mut state = state_with_status();
        let long = "工程约定与发布验收流程";
        state.memory.status = Some({
            let mut status = memory_status();
            status.workspace.files.push(file(
                "workspace",
                &format!("topics/{long}.md"),
                "topic",
                Some(1_700_000_000),
            ));
            status
        });
        let topic = entry(&state, MemoryScope::Workspace, &format!("topics/{long}.md"));
        let row = entry_row(&topic, true, 20, &Theme::no_color());
        let text: String = row.spans.iter().map(|span| span.content.as_ref()).collect();
        assert!(unicode_width::UnicodeWidthStr::width(text.as_str()) <= 20);
        assert!(text.contains('…'), "a clipped title is marked: {text:?}");
    }

    #[test]
    fn the_modal_is_centered_and_ctrl_f_uses_the_whole_terminal() {
        let area = Rect::new(0, 0, 160, 42);
        let rect = modal_rect(area, false);
        assert_eq!(rect.width, 136);
        assert_eq!(rect.height, 36);
        assert_eq!(rect.x, 12);
        assert_eq!(rect.y, 3);
        assert_eq!(modal_rect(area, true), area);
        let narrow = modal_rect(Rect::new(0, 0, 80, 24), false);
        assert_eq!(narrow.width, 68);
        assert_eq!(narrow.height, 22);
    }

    #[test]
    fn the_active_overlay_shows_both_groups_a_preview_and_no_maintenance_stats() {
        let mut state = state_with_status();
        state.memory.preview = Some(UiMemoryFileContent {
            scope: "workspace".to_owned(),
            path: "MEMORY.md".to_owned(),
            content: "# Workspace memory index\n\n- build\n".to_owned(),
            content_hash: "c".repeat(64),
            truncated: false,
            total_bytes: 34,
            error: None,
        });
        let text = rendered(&mut state, 120, 36);
        assert!(text.contains("全局"), "{text}");
        assert!(text.contains("工作区"), "{text}");
        assert!(text.contains("MEMORY.md"), "{text}");
        assert!(text.contains("build"), "{text}");
        assert!(text.contains("Ctrl+F"), "{text}");
        assert!(text.contains("阅读"), "{text}");
        assert!(!text.contains("0 个文件"), "{text}");
        assert!(!text.contains("墓碑"), "{text}");
    }

    #[test]
    fn an_enabled_empty_store_reads_as_enabled_not_disabled() {
        let mut state = state();
        let mut empty = memory_status();
        empty.global.files.clear();
        empty.workspace.files.clear();
        install_status(&mut state, empty);
        let text = rendered(&mut state, 120, 36);
        assert!(text.contains("Memory 已启用"), "{text}");
        assert!(text.contains("暂无记忆"), "{text}");
        assert!(!text.contains("未启用"), "{text}");
    }

    #[test]
    fn the_off_state_names_the_loaded_config_path_and_shows_no_fake_list() {
        let mut state = state();
        let mut off = memory_status();
        off.rollout = "off".to_owned();
        off.enabled = false;
        off.config_path = "/tmp/isolated-home/config.toml".to_owned();
        off.global.files.clear();
        off.workspace.files.clear();
        install_status(&mut state, off);
        let text = rendered(&mut state, 120, 36);
        assert!(text.contains("/tmp/isolated-home/config.toml"), "{text}");
        assert!(text.contains("[memory]"), "{text}");
        assert!(text.contains("rollout = \"active\""), "{text}");
        assert!(!text.contains("全局"), "{text}");
        assert!(!text.contains("工作区"), "{text}");
    }

    #[test]
    fn the_paused_overlay_keeps_the_files_and_offers_resume() {
        let mut state = state_with_status();
        let mut paused = memory_status();
        paused.rollout = "shadow".to_owned();
        paused.enabled = false;
        paused.capture_enabled = false;
        install_status(&mut state, paused);
        let text = rendered(&mut state, 120, 36);
        assert!(text.contains("已暂停"), "{text}");
        assert!(text.contains("t 恢复"), "{text}");
        assert!(text.contains("x 遗忘"), "{text}");
        assert!(!text.contains("f 立即捕获"), "{text}");
    }

    #[test]
    fn action_feedback_is_visible_above_the_footer() {
        let mut state = state_with_status();
        select(&mut state, MemoryScope::Workspace, "topics/build.md");
        state.memory.preview = Some(UiMemoryFileContent {
            scope: "workspace".to_owned(),
            path: "topics/build.md".to_owned(),
            content: "note".to_owned(),
            content_hash: "a".repeat(64),
            truncated: false,
            total_bytes: 4,
            error: None,
        });
        press_forget(&mut state);
        let text = rendered(&mut state, 120, 36);
        assert!(text.contains("再按一次 x 遗忘"), "{text}");

        state.memory.notice = None;
        state.memory.error = Some("boom".to_owned());
        let text = rendered(&mut state, 120, 36);
        assert!(text.contains("错误：boom"), "{text}");
    }

    #[test]
    fn the_off_footer_does_not_advertise_capture_or_dream() {
        let mut state = state();
        let mut off = memory_status();
        off.rollout = "off".to_owned();
        off.enabled = false;
        install_status(&mut state, off);
        let text = rendered(&mut state, 120, 36);
        assert!(text.contains("Esc 关闭"), "{text}");
        assert!(!text.contains("立即捕获"), "{text}");
        assert!(!text.contains("整理"), "{text}");
    }

    #[test]
    fn a_scope_read_error_is_shown_even_while_searching() {
        let mut state = state();
        let mut broken = memory_status();
        broken.workspace.files.clear();
        broken.workspace.error = Some("store is unreadable".to_owned());
        install_status(&mut state, broken);
        state.memory.filter = "anything".to_owned();
        clamp_selection(&mut state);
        let text = rendered(&mut state, 120, 36);
        assert!(text.contains("store is unreadable"), "{text}");
        assert!(!text.contains("没有匹配当前搜索"), "{text}");
    }

    #[test]
    fn a_narrow_overlay_wraps_the_footer_instead_of_clipping_it() {
        let mut state = state_with_status();
        let text = rendered(&mut state, 80, 24);
        assert!(text.contains("Esc 关闭"), "{text}");
        assert!(text.contains("x 遗忘"), "{text}");
    }
}
