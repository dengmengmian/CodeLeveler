//! `/mcp` — MCP server status and management.
//!
//! Every connection fact on this page comes from the runtime's one MCP
//! authority ([`ClientCommand::ManageMcp`]). The client never infers
//! "connected" from configuration, never parses log text, and never merges a
//! stale answer over a newer generation. Configuration edits travel through
//! the same command, which writes the global config with the runtime's own
//! editor and reloads it; a reload that fails is shown as a failure.
//!
//! The page opens while a turn runs (status is read-only), but every mutating
//! action is refused until the turn settles: changing the catalog under an
//! executing turn would change the tool surface it is already using. The
//! runtime enforces the same rule, so a second client cannot bypass it.

use std::time::{Duration, Instant};

use crossterm::event::{KeyCode, KeyEvent, MouseEvent, MouseEventKind};
use leveler_client_protocol::mcp::{
    McpConnectionState, McpNameRef, McpRequest, McpResponse, McpServerSpec, McpServerView,
    McpToolAdmission, McpTransport,
};
use leveler_client_protocol::{ClientCommand, CommandId, RuntimeEvent, SessionId};
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use unicode_width::UnicodeWidthStr;

use crate::action::Effect;
use crate::state::AppState;

/// A request that never answers must not look like "loading" forever. The
/// answer still applies if it lands later; it is a fact.
const QUERY_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum McpTab {
    #[default]
    Servers,
    Tools,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum McpIntent {
    List,
    Reload,
    Reconnect,
    Add,
    Update,
    Remove,
    SetEnabled { enabled: bool },
}

#[derive(Debug, Default)]
pub struct McpState {
    pub servers: Vec<McpServerView>,
    /// The one answer this page accepts.
    pub query: Option<CommandId>,
    pub query_started: Option<Instant>,
    /// Stable selection by server name.
    pub selected: Option<String>,
    pub tab: McpTab,
    pub detail_open: bool,
    pub pending: Option<McpIntent>,
    pub notice: Option<String>,
    pub error: Option<String>,
    pub form: Option<McpForm>,
    pub confirm_remove: Option<String>,
    pub filter: String,
    pub filter_focus: bool,
    pub tool_scroll: usize,
}

impl McpState {
    fn checking(&self) -> bool {
        self.query.is_some()
            && self
                .query_started
                .is_some_and(|at| at.elapsed() < QUERY_TIMEOUT)
    }

    fn timed_out(&self) -> bool {
        self.query.is_some()
            && self
                .query_started
                .is_some_and(|at| at.elapsed() >= QUERY_TIMEOUT)
    }
}

/// The runtime refused a management command (or never answered it). Release
/// the page's in-flight latch: leaving it armed makes Enter look ignored and
/// Esc dead, and the reason must be visible on the form that issued it.
pub fn observe_command_failure(state: &mut AppState, message: Option<&str>) {
    let message = message
        .map(str::to_string)
        .unwrap_or_else(|| state.t().mcp_page.query_failed.to_string());
    state.mcp.query = None;
    state.mcp.query_started = None;
    state.mcp.pending = None;
    state.mcp.error = Some(message.clone());
    if let Some(form) = state.mcp.form.as_mut() {
        form.submitting = false;
        form.error = Some(message);
    }
}

// ---------------------------------------------------------------------------
// Opening and requests
// ---------------------------------------------------------------------------

pub fn open(state: &mut AppState) -> Vec<Effect> {
    if state.active_screen == crate::screen::Screen::Mcp {
        close(state);
        return Vec::new();
    }
    state.active_screen = crate::screen::Screen::Mcp;
    state.screen_scroll = 0;
    state.detail_return = None;
    state.mcp.detail_open = false;
    state.mcp.tab = McpTab::Servers;
    state.mcp.tool_scroll = 0;
    ensure_selection(state);
    reload(state)
}

/// One level of back, with the same precedence the Esc key uses: form →
/// remove confirmation → server/tool detail → filter → the page itself.
/// Returns `true` when it consumed the level; `false` means the caller should
/// close the page. A submitting form is inert, exactly like Esc.
pub fn back(state: &mut AppState) -> bool {
    if let Some(form) = state.mcp.form.as_ref() {
        if !form.submitting {
            state.mcp.form = None;
        }
        return true;
    }
    if state.mcp.confirm_remove.is_some() {
        state.mcp.confirm_remove = None;
        return true;
    }
    if state.mcp.detail_open {
        state.mcp.detail_open = false;
        state.mcp.tool_scroll = 0;
        return true;
    }
    if state.mcp.filter_focus {
        state.mcp.filter_focus = false;
        if !state.mcp.filter.is_empty() {
            state.mcp.filter.clear();
            ensure_selection(state);
        }
        return true;
    }
    false
}

pub fn close(state: &mut AppState) {
    state.active_screen = crate::screen::Screen::Conversation;
    state.screen_scroll = 0;
    state.mcp.query = None;
    state.mcp.query_started = None;
    state.mcp.form = None;
    state.mcp.confirm_remove = None;
    state.mcp.filter.clear();
    state.mcp.filter_focus = false;
    state.mcp.tool_scroll = 0;
}

/// Read the authoritative list. Never connects a server.
pub fn reload(state: &mut AppState) -> Vec<Effect> {
    send(state, McpRequest::List, McpIntent::List)
}

fn send(state: &mut AppState, request: McpRequest, intent: McpIntent) -> Vec<Effect> {
    expire_failed_query(state);
    if state.mcp.query.is_some() {
        return Vec::new();
    }
    if !state.runtime_connected {
        state.mcp.error = Some(state.t().mcp_page.disconnected.to_string());
        return Vec::new();
    }
    let query_id = CommandId::generate();
    state.mcp.query = Some(query_id.clone());
    state.mcp.query_started = Some(Instant::now());
    state.mcp.pending = Some(intent);
    state.mcp.error = None;
    vec![Effect::Send(ClientCommand::ManageMcp {
        requester_session_id: state.session_id.clone(),
        query_id,
        request: Box::new(request),
    })]
}

/// A timed-out answer no longer owns the page. Clearing it lets `r` and a
/// mutation retry instead of being silently refused for the rest of the visit.
fn expire_failed_query(state: &mut AppState) {
    if state.mcp.query.is_some() && !state.mcp.checking() {
        state.mcp.query = None;
        state.mcp.query_started = None;
        state.mcp.pending = None;
        state.mcp.error = Some(state.t().mcp_page.query_failed.to_string());
    }
}

/// A mutation is refused while a turn runs or another request is in flight.
fn mutate(state: &mut AppState, request: McpRequest, intent: McpIntent) -> Vec<Effect> {
    expire_failed_query(state);
    if state.is_busy() {
        state.mcp.notice = None;
        state.mcp.error = Some(state.t().mcp_page.busy_notice.to_string());
        return Vec::new();
    }
    if state.mcp.query.is_some() {
        return Vec::new();
    }
    send(state, request, intent)
}

/// Accept only the answer belonging to this client and this query.
pub fn apply_managed(
    state: &mut AppState,
    requester_session_id: &SessionId,
    query_id: &CommandId,
    response: McpResponse,
) {
    if requester_session_id != &state.session_id {
        return;
    }
    if state.mcp.query.as_ref() != Some(query_id) {
        return;
    }
    let intent = state.mcp.pending.take();
    state.mcp.query = None;
    state.mcp.query_started = None;
    match response {
        McpResponse::Snapshot { servers } => {
            state.mcp.servers = servers;
            state.mcp.error = None;
            // A fresh list establishes the rows the keyboard acts on. Without
            // this, the first `r`/`e`/`x` after opening the page had no
            // selected server and silently did nothing.
            ensure_selection(state);
        }
        McpResponse::Changed { servers, .. } => {
            state.mcp.servers = servers;
            state.mcp.error = None;
            state.mcp.notice = Some(intent_notice(state, intent));
            state.mcp.form = None;
            state.mcp.confirm_remove = None;
            ensure_selection(state);
        }
        McpResponse::Error { servers, message } => {
            state.mcp.servers = servers;
            state.mcp.error = Some(message.clone());
            if let Some(form) = state.mcp.form.as_mut() {
                form.submitting = false;
                form.error = Some(message);
            }
            ensure_selection(state);
        }
    }
}

fn intent_notice(state: &AppState, intent: Option<McpIntent>) -> String {
    let t = &state.t().mcp_page;
    match intent {
        Some(McpIntent::Reload) => t.notice_reloaded.to_string(),
        Some(McpIntent::Reconnect) => t.notice_reconnected.to_string(),
        Some(McpIntent::Add) => t.notice_added.to_string(),
        Some(McpIntent::Update) => t.notice_updated.to_string(),
        Some(McpIntent::Remove) => t.notice_removed.to_string(),
        Some(McpIntent::SetEnabled { enabled: true }) => t.notice_enabled.to_string(),
        Some(McpIntent::SetEnabled { enabled: false }) => t.notice_disabled.to_string(),
        Some(McpIntent::List) | None => String::new(),
    }
}

/// A session switch drops the rows: their state belongs to the runtime
/// authority, and the page re-reads if it is open.
pub fn session_changed(state: &mut AppState) -> Vec<Effect> {
    state.mcp.servers.clear();
    state.mcp.query = None;
    state.mcp.query_started = None;
    state.mcp.selected = None;
    state.mcp.form = None;
    state.mcp.confirm_remove = None;
    ensure_selection(state);
    if state.active_screen == crate::screen::Screen::Mcp {
        reload(state)
    } else {
        Vec::new()
    }
}

/// A reconnect or a runtime restart makes the rows older than the fact.
pub fn should_reload(event: &RuntimeEvent) -> bool {
    matches!(event, RuntimeEvent::RuntimeReconnected { .. })
}

// ---------------------------------------------------------------------------
// Selection and rows
// ---------------------------------------------------------------------------

fn visible_servers(state: &AppState) -> Vec<&McpServerView> {
    let filter = state.mcp.filter.trim().to_lowercase();
    state
        .mcp
        .servers
        .iter()
        .filter(|server| {
            filter.is_empty()
                || server.name.to_lowercase().contains(&filter)
                || server.endpoint.to_lowercase().contains(&filter)
                || state_label(state, server.state)
                    .to_lowercase()
                    .contains(&filter)
        })
        .collect()
}

fn selected_server(state: &AppState) -> Option<&McpServerView> {
    let name = state.mcp.selected.as_deref()?;
    state.mcp.servers.iter().find(|server| server.name == name)
}

fn ensure_selection(state: &mut AppState) {
    let visible = visible_servers(state);
    let Some(first) = visible.first() else {
        state.mcp.selected = None;
        return;
    };
    if let Some(selected) = &state.mcp.selected
        && visible.iter().any(|server| &server.name == selected)
    {
        return;
    }
    state.mcp.selected = Some(first.name.clone());
}

fn move_selection(state: &mut AppState, delta: isize) {
    let visible = visible_servers(state);
    if visible.is_empty() {
        state.mcp.selected = None;
        return;
    }
    let current = state
        .mcp
        .selected
        .as_deref()
        .and_then(|name| visible.iter().position(|server| server.name == name))
        .unwrap_or(0);
    let next = if delta < 0 {
        current.saturating_sub(delta.unsigned_abs())
    } else {
        (current + delta as usize).min(visible.len() - 1)
    };
    state.mcp.selected = Some(visible[next].name.clone());
    state.mcp.tool_scroll = 0;
}

// ---------------------------------------------------------------------------
// Keys
// ---------------------------------------------------------------------------

pub fn handle_key(state: &mut AppState, key: KeyEvent) -> Vec<Effect> {
    if state.mcp.form.is_some() {
        return handle_form_key(state, key);
    }
    if state.mcp.confirm_remove.is_some() {
        return handle_confirm_key(state, key);
    }
    if state.mcp.detail_open {
        return handle_detail_key(state, key);
    }
    handle_list_key(state, key)
}

fn handle_list_key(state: &mut AppState, key: KeyEvent) -> Vec<Effect> {
    if state.mcp.filter_focus {
        match key.code {
            KeyCode::Esc => {
                state.mcp.filter_focus = false;
                if state.mcp.filter.is_empty() {
                    return Vec::new();
                }
                state.mcp.filter.clear();
                ensure_selection(state);
                Vec::new()
            }
            KeyCode::Enter => {
                state.mcp.filter_focus = false;
                Vec::new()
            }
            KeyCode::Backspace => {
                pop_char(&mut state.mcp.filter);
                ensure_selection(state);
                Vec::new()
            }
            KeyCode::Char(ch)
                if !key
                    .modifiers
                    .intersects(crossterm::event::KeyModifiers::CONTROL) =>
            {
                state.mcp.filter.push(ch);
                ensure_selection(state);
                Vec::new()
            }
            _ => Vec::new(),
        }
    } else {
        match key.code {
            KeyCode::Esc => {
                close(state);
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
            KeyCode::PageUp => {
                move_selection(state, -10);
                Vec::new()
            }
            KeyCode::PageDown => {
                move_selection(state, 10);
                Vec::new()
            }
            KeyCode::Enter => {
                if selected_server(state).is_some() {
                    state.mcp.detail_open = true;
                    state.mcp.tool_scroll = 0;
                }
                Vec::new()
            }
            KeyCode::Tab | KeyCode::BackTab => {
                state.mcp.tab = match state.mcp.tab {
                    McpTab::Servers => McpTab::Tools,
                    McpTab::Tools => McpTab::Servers,
                };
                state.mcp.tool_scroll = 0;
                Vec::new()
            }
            KeyCode::Char('/') => {
                state.mcp.filter_focus = true;
                Vec::new()
            }
            KeyCode::Char('a') => {
                begin_add(state);
                Vec::new()
            }
            KeyCode::Char('e') => {
                begin_edit(state);
                Vec::new()
            }
            KeyCode::Char('d') => {
                begin_remove(state);
                Vec::new()
            }
            KeyCode::Char('x') => toggle_enabled(state),
            KeyCode::Char('r') => reconnect_selected(state),
            KeyCode::Char('R') => reload_all(state),
            _ => Vec::new(),
        }
    }
}

fn handle_detail_key(state: &mut AppState, key: KeyEvent) -> Vec<Effect> {
    match key.code {
        KeyCode::Esc => {
            state.mcp.detail_open = false;
            state.mcp.tool_scroll = 0;
            Vec::new()
        }
        KeyCode::Tab | KeyCode::BackTab => {
            state.mcp.tab = match state.mcp.tab {
                McpTab::Servers => McpTab::Tools,
                McpTab::Tools => McpTab::Servers,
            };
            state.mcp.tool_scroll = 0;
            Vec::new()
        }
        KeyCode::Up | KeyCode::Char('k') => {
            state.mcp.tool_scroll = state.mcp.tool_scroll.saturating_sub(1);
            Vec::new()
        }
        KeyCode::Down | KeyCode::Char('j') => {
            state.mcp.tool_scroll = state.mcp.tool_scroll.saturating_add(1);
            Vec::new()
        }
        KeyCode::PageUp => {
            state.mcp.tool_scroll = state.mcp.tool_scroll.saturating_sub(10);
            Vec::new()
        }
        KeyCode::PageDown => {
            state.mcp.tool_scroll = state.mcp.tool_scroll.saturating_add(10);
            Vec::new()
        }
        KeyCode::Char('r') => reconnect_selected(state),
        KeyCode::Char('R') => reload_all(state),
        KeyCode::Char('a') => {
            begin_add(state);
            Vec::new()
        }
        KeyCode::Char('e') => {
            begin_edit(state);
            Vec::new()
        }
        KeyCode::Char('d') => {
            begin_remove(state);
            Vec::new()
        }
        KeyCode::Char('x') => toggle_enabled(state),
        _ => Vec::new(),
    }
}

fn handle_confirm_key(state: &mut AppState, key: KeyEvent) -> Vec<Effect> {
    let Some(name) = state.mcp.confirm_remove.clone() else {
        return Vec::new();
    };
    match key.code {
        KeyCode::Esc | KeyCode::Char('n') | KeyCode::Char('N') => {
            state.mcp.confirm_remove = None;
            Vec::new()
        }
        KeyCode::Enter | KeyCode::Char('y') | KeyCode::Char('Y') => {
            state.mcp.confirm_remove = None;
            mutate(
                state,
                McpRequest::Remove { server: name },
                McpIntent::Remove,
            )
        }
        _ => Vec::new(),
    }
}

fn reconnect_selected(state: &mut AppState) -> Vec<Effect> {
    let Some(server) = selected_server(state) else {
        return Vec::new();
    };
    let name = server.name.clone();
    if !server.enabled {
        state.mcp.error = Some(state.t().mcp_page.state_disabled.to_string());
        return Vec::new();
    }
    mutate(
        state,
        McpRequest::Reconnect { server: name },
        McpIntent::Reconnect,
    )
}

fn reload_all(state: &mut AppState) -> Vec<Effect> {
    mutate(state, McpRequest::Reload, McpIntent::Reload)
}

fn toggle_enabled(state: &mut AppState) -> Vec<Effect> {
    let Some(server) = selected_server(state) else {
        return Vec::new();
    };
    let name = server.name.clone();
    let enabled = !server.enabled;
    mutate(
        state,
        McpRequest::SetEnabled {
            server: name,
            enabled,
        },
        McpIntent::SetEnabled { enabled },
    )
}

fn begin_remove(state: &mut AppState) {
    if selected_server(state).is_some() {
        state.mcp.confirm_remove = state.mcp.selected.clone();
    }
}

fn begin_add(state: &mut AppState) {
    state.mcp.form = Some(McpForm::new());
}

fn begin_edit(state: &mut AppState) {
    let Some(server) = selected_server(state) else {
        return;
    };
    state.mcp.form = Some(McpForm::from_server(server));
}

// ---------------------------------------------------------------------------
// Form
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum McpField {
    Name,
    Transport,
    Command,
    Args,
    Url,
    Env,
    Headers,
    Bearer,
    Enabled,
}

const STDIO_FIELDS: &[McpField] = &[
    McpField::Name,
    McpField::Transport,
    McpField::Command,
    McpField::Args,
    McpField::Env,
    McpField::Enabled,
];

const HTTP_FIELDS: &[McpField] = &[
    McpField::Name,
    McpField::Transport,
    McpField::Url,
    McpField::Headers,
    McpField::Bearer,
    McpField::Enabled,
];

#[derive(Debug, Clone)]
pub struct McpForm {
    pub editing: Option<String>,
    pub focus: McpField,
    pub cursor: usize,
    pub name: String,
    pub transport: McpTransport,
    pub command: String,
    pub args: String,
    pub url: String,
    pub env: String,
    pub headers: String,
    pub bearer: String,
    pub enabled: bool,
    pub submitting: bool,
    pub error: Option<String>,
}

impl McpForm {
    fn new() -> Self {
        Self {
            editing: None,
            focus: McpField::Name,
            cursor: 0,
            name: String::new(),
            transport: McpTransport::Stdio,
            command: String::new(),
            args: String::new(),
            url: String::new(),
            env: String::new(),
            headers: String::new(),
            bearer: String::new(),
            enabled: true,
            submitting: false,
            error: None,
        }
    }

    fn from_server(server: &McpServerView) -> Self {
        let spec = &server.spec;
        Self {
            editing: Some(server.name.clone()),
            focus: McpField::Name,
            cursor: server.name.chars().count(),
            name: server.name.clone(),
            transport: spec.transport,
            command: spec.command.clone().unwrap_or_default(),
            args: spec.args.join(" "),
            url: spec.url.clone().unwrap_or_default(),
            env: format_refs(&spec.env),
            headers: format_refs(&spec.headers),
            bearer: spec.bearer_token_env.clone().unwrap_or_default(),
            enabled: spec.enabled,
            submitting: false,
            error: None,
        }
    }

    fn fields(&self) -> &'static [McpField] {
        match self.transport {
            McpTransport::Stdio => STDIO_FIELDS,
            McpTransport::StreamableHttp => HTTP_FIELDS,
        }
    }

    fn field_index(&self) -> usize {
        self.fields()
            .iter()
            .position(|field| *field == self.focus)
            .unwrap_or(0)
    }

    fn is_last(&self) -> bool {
        self.field_index() + 1 == self.fields().len()
    }

    fn text_mut(&mut self, field: McpField) -> Option<&mut String> {
        match field {
            McpField::Name => Some(&mut self.name),
            McpField::Command => Some(&mut self.command),
            McpField::Args => Some(&mut self.args),
            McpField::Url => Some(&mut self.url),
            McpField::Env => Some(&mut self.env),
            McpField::Headers => Some(&mut self.headers),
            McpField::Bearer => Some(&mut self.bearer),
            McpField::Transport | McpField::Enabled => None,
        }
    }

    fn text(&self, field: McpField) -> Option<&String> {
        match field {
            McpField::Name => Some(&self.name),
            McpField::Command => Some(&self.command),
            McpField::Args => Some(&self.args),
            McpField::Url => Some(&self.url),
            McpField::Env => Some(&self.env),
            McpField::Headers => Some(&self.headers),
            McpField::Bearer => Some(&self.bearer),
            McpField::Transport | McpField::Enabled => None,
        }
    }

    fn move_focus(&mut self, forward: bool) {
        let fields = self.fields();
        let index = self.field_index();
        let next = if forward {
            (index + 1).min(fields.len() - 1)
        } else {
            index.saturating_sub(1)
        };
        self.focus = fields[next];
        self.cursor = self
            .text(self.focus)
            .map(|text| text.chars().count())
            .unwrap_or(0);
    }

    fn insert_char(&mut self, ch: char) {
        let cursor = self.cursor;
        if let Some(text) = self.text_mut(self.focus) {
            let byte = text
                .char_indices()
                .nth(cursor)
                .map(|(byte, _)| byte)
                .unwrap_or(text.len());
            text.insert(byte, ch);
            self.cursor += 1;
        }
    }

    fn backspace(&mut self) {
        if self.cursor == 0 {
            return;
        }
        let cursor = self.cursor;
        if let Some(text) = self.text_mut(self.focus)
            && let Some((byte, _)) = text.char_indices().nth(cursor - 1)
        {
            text.remove(byte);
        }
        self.cursor -= 1;
    }

    fn delete(&mut self) {
        let cursor = self.cursor;
        if let Some(text) = self.text_mut(self.focus)
            && let Some((byte, _)) = text.char_indices().nth(cursor)
        {
            text.remove(byte);
        }
    }

    fn toggle_transport(&mut self) {
        self.transport = match self.transport {
            McpTransport::Stdio => McpTransport::StreamableHttp,
            McpTransport::StreamableHttp => McpTransport::Stdio,
        };
        self.focus = McpField::Name;
        self.cursor = self.name.chars().count();
    }
}

fn handle_form_key(state: &mut AppState, key: KeyEvent) -> Vec<Effect> {
    let Some(form) = state.mcp.form.as_mut() else {
        return Vec::new();
    };
    if form.submitting {
        return Vec::new();
    }
    match key.code {
        KeyCode::Esc => {
            state.mcp.form = None;
            Vec::new()
        }
        KeyCode::Up => {
            form.move_focus(false);
            Vec::new()
        }
        KeyCode::Down | KeyCode::Tab if !form.is_last() => {
            form.move_focus(true);
            Vec::new()
        }
        KeyCode::Left | KeyCode::Right => {
            if form.focus == McpField::Transport {
                form.toggle_transport();
            } else if form.focus == McpField::Enabled {
                form.enabled = !form.enabled;
            }
            Vec::new()
        }
        KeyCode::Home => {
            form.cursor = 0;
            Vec::new()
        }
        KeyCode::End => {
            form.cursor = form
                .text(form.focus)
                .map(|text| text.chars().count())
                .unwrap_or(0);
            Vec::new()
        }
        KeyCode::Backspace => {
            form.backspace();
            Vec::new()
        }
        KeyCode::Delete => {
            form.delete();
            Vec::new()
        }
        KeyCode::Char(' ') if form.focus == McpField::Enabled => {
            form.enabled = !form.enabled;
            Vec::new()
        }
        KeyCode::Enter => {
            if form.focus == McpField::Enabled || form.is_last() {
                submit_form(state)
            } else {
                form.move_focus(true);
                Vec::new()
            }
        }
        KeyCode::Char(ch)
            if !key
                .modifiers
                .intersects(crossterm::event::KeyModifiers::CONTROL)
                && form.focus != McpField::Transport
                && form.focus != McpField::Enabled =>
        {
            form.insert_char(ch);
            Vec::new()
        }
        _ => Vec::new(),
    }
}

fn submit_form(state: &mut AppState) -> Vec<Effect> {
    let Some(form) = state.mcp.form.clone() else {
        return Vec::new();
    };
    let t = &state.t().mcp_page;
    let name = form.name.trim().to_string();
    if name.is_empty() {
        set_form_error(state, t.error_required_name);
        return Vec::new();
    }
    let (command, args, env, url, headers, bearer) = match form.transport {
        McpTransport::Stdio => {
            let command = form.command.trim().to_string();
            if command.is_empty() {
                set_form_error(state, t.error_required_command);
                return Vec::new();
            }
            let Ok(env) = parse_refs(&form.env, true) else {
                set_form_error(state, t.error_invalid_refs);
                return Vec::new();
            };
            (
                Some(command),
                split_words(&form.args),
                env,
                None,
                Vec::new(),
                None,
            )
        }
        McpTransport::StreamableHttp => {
            let url = form.url.trim().to_string();
            if url.is_empty() {
                set_form_error(state, t.error_required_url);
                return Vec::new();
            }
            let Ok(headers) = parse_refs(&form.headers, false) else {
                set_form_error(state, t.error_invalid_refs);
                return Vec::new();
            };
            if headers
                .iter()
                .any(|reference| reference.key.eq_ignore_ascii_case("authorization"))
            {
                set_form_error(state, t.error_invalid_refs);
                return Vec::new();
            }
            let bearer = {
                let value = form.bearer.trim();
                (!value.is_empty()).then(|| value.to_string())
            };
            (None, Vec::new(), Vec::new(), Some(url), headers, bearer)
        }
    };
    let spec = McpServerSpec {
        name,
        transport: form.transport,
        command,
        args,
        env,
        url,
        headers,
        bearer_token_env: bearer,
        enabled: form.enabled,
    };
    // The mutation gate runs before the form is latched. Otherwise a refused
    // submit (busy turn, in-flight query, disconnected runtime) leaves the
    // form on "submitting": Enter appears ignored, Esc is blocked, and the
    // reason is hidden behind the form.
    if state.is_busy() {
        let message = state.t().mcp_page.busy_notice.to_string();
        set_form_error(state, &message);
        return Vec::new();
    }
    expire_failed_query(state);
    if state.mcp.query.is_some() {
        let message = state.t().mcp_page.pending.to_string();
        set_form_error(state, &message);
        return Vec::new();
    }
    if !state.runtime_connected {
        let message = state.t().mcp_page.disconnected.to_string();
        set_form_error(state, &message);
        return Vec::new();
    }
    if let Some(form) = state.mcp.form.as_mut() {
        form.submitting = true;
        form.error = None;
    }
    match form.editing {
        Some(original_name) => mutate(
            state,
            McpRequest::Update {
                original_name,
                spec,
            },
            McpIntent::Update,
        ),
        None => mutate(state, McpRequest::Add { spec }, McpIntent::Add),
    }
}

fn set_form_error(state: &mut AppState, message: &str) {
    if let Some(form) = state.mcp.form.as_mut() {
        form.submitting = false;
        form.error = Some(message.to_string());
    }
}

/// A quote-aware split for the arguments field. Arguments may contain spaces
/// inside double quotes; a backslash escapes the next character.
fn split_words(input: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut current = String::new();
    let mut quoted = false;
    let mut escaped = false;
    for ch in input.chars() {
        if escaped {
            current.push(ch);
            escaped = false;
            continue;
        }
        match ch {
            '\\' => escaped = true,
            '"' => quoted = !quoted,
            c if c.is_whitespace() && !quoted => {
                if !current.is_empty() {
                    out.push(std::mem::take(&mut current));
                }
            }
            c => current.push(c),
        }
    }
    if !current.is_empty() {
        out.push(current);
    }
    out
}

/// Parse `KEY=SOURCE` pairs separated by commas or newlines. A bare key means
/// the source has the same name (a valid environment reference).
fn parse_refs(input: &str, allow_bare_key: bool) -> Result<Vec<McpNameRef>, ()> {
    let mut out = Vec::new();
    for part in input.split([',', '\n', ';']) {
        let part = part.trim();
        if part.is_empty() {
            continue;
        }
        let (key, source) = match part.split_once('=') {
            Some((key, source)) => (key.trim(), source.trim()),
            None if allow_bare_key => (part, part),
            None => return Err(()),
        };
        if key.is_empty() || source.is_empty() {
            return Err(());
        }
        out.push(McpNameRef {
            key: key.to_string(),
            source: source.to_string(),
        });
    }
    Ok(out)
}

fn format_refs(refs: &[McpNameRef]) -> String {
    refs.iter()
        .map(|reference| format!("{}={}", reference.key, reference.source))
        .collect::<Vec<_>>()
        .join(", ")
}

fn pop_char(text: &mut String) {
    if let Some((index, _)) = text.char_indices().next_back() {
        text.truncate(index);
    }
}

// ---------------------------------------------------------------------------
// Mouse
// ---------------------------------------------------------------------------

pub fn handle_mouse(state: &mut AppState, mouse: MouseEvent) -> Vec<Effect> {
    match mouse.kind {
        MouseEventKind::ScrollUp => {
            if state.mcp.detail_open {
                state.mcp.tool_scroll = state.mcp.tool_scroll.saturating_sub(3);
            } else {
                move_selection(state, -1);
            }
            Vec::new()
        }
        MouseEventKind::ScrollDown => {
            if state.mcp.detail_open {
                state.mcp.tool_scroll = state.mcp.tool_scroll.saturating_add(3);
            } else {
                move_selection(state, 1);
            }
            Vec::new()
        }
        MouseEventKind::Down(crossterm::event::MouseButton::Left) => {
            let point = (mouse.column, mouse.row).into();
            if state.mcp.form.is_some() || state.mcp.confirm_remove.is_some() {
                return Vec::new();
            }
            if let Some((_, key)) = state
                .screen_rows
                .iter()
                .find(|(rect, _)| rect.contains(point))
            {
                // A row click selects; Enter is this page's one detail binding,
                // the same rule every secondary page shares.
                state.mcp.selected = Some(key.clone());
                state.mcp.detail_open = false;
            }
            Vec::new()
        }
        _ => Vec::new(),
    }
}

// ---------------------------------------------------------------------------
// Rendering
// ---------------------------------------------------------------------------

fn state_label(state: &AppState, value: McpConnectionState) -> &'static str {
    let t = &state.t().mcp_page;
    match value {
        McpConnectionState::Disabled => t.state_disabled,
        McpConnectionState::NotConnected => t.state_not_connected,
        McpConnectionState::Connecting => t.state_connecting,
        McpConnectionState::Connected => t.state_connected,
        McpConnectionState::Degraded => t.state_degraded,
        McpConnectionState::Failed => t.state_failed,
        McpConnectionState::Unknown => t.state_unknown,
    }
}

fn state_color(state: &AppState, value: McpConnectionState) -> ratatui::style::Color {
    match value {
        McpConnectionState::Connected => state.theme.status.success,
        McpConnectionState::Failed => state.theme.status.error,
        McpConnectionState::Degraded | McpConnectionState::NotConnected => {
            state.theme.status.warning
        }
        McpConnectionState::Connecting => state.theme.status.info,
        McpConnectionState::Disabled | McpConnectionState::Unknown => state.theme.text.muted,
    }
}

fn admission_label(state: &AppState, value: McpToolAdmission) -> &'static str {
    let t = &state.t().mcp_page;
    match value {
        McpToolAdmission::Admitted => t.admission_admitted,
        McpToolAdmission::RequiresApproval => t.admission_requires_approval,
        McpToolAdmission::RequiresNetworkGrant => t.admission_requires_network,
        McpToolAdmission::Unknown => t.admission_unknown,
    }
}

fn pad(text: &str, width: usize) -> String {
    let used = UnicodeWidthStr::width(text);
    let mut out = text.to_string();
    for _ in used..width {
        out.push(' ');
    }
    out
}

pub fn render_screen(frame: &mut Frame, area: Rect, state: &mut AppState) {
    let t = &state.t().mcp_page;
    let hint = if state.mcp.form.is_some() {
        t.footer_form
    } else if state.mcp.detail_open {
        t.footer_detail
    } else {
        t.footer_list
    };
    let page = crate::secondary::SecondaryPage {
        title: t.title,
        status: crate::secondary::main_status_spans(state, area.width as usize),
        hint,
    };
    let layout = crate::secondary::layout(area, 2);
    crate::secondary::draw_header(frame, state, &layout, &page);
    crate::secondary::draw_footer(frame, &layout, &page, &state.theme);
    if state.mcp.form.is_some() {
        render_form(frame, layout.content, state);
        return;
    }
    render_filter(frame, layout.composer, state);
    let list_visible = layout.content.width >= 80 || !state.mcp.detail_open;
    if list_visible {
        render_list(frame, layout.content, state);
    }
    if state.mcp.detail_open || layout.content.width >= 80 {
        render_detail(frame, layout.content, state, !list_visible);
    }
    if !list_visible {
        // The list did not paint rows: its hit targets must not stay live.
        state.screen_rows.clear();
    }
    if state.mcp.confirm_remove.is_some() {
        render_confirm(frame, layout.content, state);
    }
}

fn render_filter(frame: &mut Frame, area: Rect, state: &AppState) {
    if area.height == 0 || area.width == 0 {
        return;
    }
    let t = &state.t().mcp_page;
    let tabs = format!(
        "[{}] {}   [{}] {}",
        if state.mcp.tab == McpTab::Servers {
            "*"
        } else {
            " "
        },
        t.tabs_servers,
        if state.mcp.tab == McpTab::Tools {
            "*"
        } else {
            " "
        },
        t.tabs_tools
    );
    let filter = format!("{}{}", t.filter_label, state.mcp.filter);
    let line = format!("{filter}    {tabs}");
    let style = Style::default().fg(state.theme.text.muted);
    frame.render_widget(
        Paragraph::new(crate::render::text::sanitize_terminal_line(&line)).style(style),
        area,
    );
    if state.mcp.filter_focus {
        let cursor_text = crate::render::text::sanitize_terminal_line(&format!(
            "{}{}",
            t.filter_label, state.mcp.filter
        ));
        let x =
            UnicodeWidthStr::width(cursor_text.as_str()).min(area.width.saturating_sub(1) as usize);
        frame.set_cursor_position((area.x + x as u16, area.y));
    }
    let mut status = Vec::new();
    if state.mcp.checking() {
        status.push(t.pending.to_string());
    } else if state.mcp.timed_out() {
        status.push(t.query_failed.to_string());
    }
    if let Some(notice) = &state.mcp.notice {
        status.push(notice.clone());
    }
    if let Some(error) = &state.mcp.error {
        status.push(error.clone());
    }
    if !status.is_empty() && area.height > 1 {
        let color = if state.mcp.error.is_some() {
            state.theme.status.error
        } else {
            state.theme.status.info
        };
        frame.render_widget(
            Paragraph::new(crate::render::text::sanitize_terminal_line(
                &status.join("  ·  "),
            ))
            .style(Style::default().fg(color)),
            Rect {
                y: area.y + 1,
                height: 1,
                ..area
            },
        );
    }
}

fn render_list(frame: &mut Frame, area: Rect, state: &mut AppState) {
    let t = &state.t().mcp_page;
    let theme = state.theme.clone();
    if area.width == 0 || area.height == 0 {
        return;
    }
    let list_area = if area.width >= 80 {
        Rect {
            width: area.width * 44 / 100,
            ..area
        }
    } else {
        area
    };
    let visible = visible_servers(state);
    if visible.is_empty() {
        let message = if state.mcp.servers.is_empty() {
            t.no_servers
        } else {
            t.no_matches
        };
        frame.render_widget(
            Paragraph::new(crate::render::text::sanitize_terminal_line(message))
                .style(Style::default().fg(theme.text.muted)),
            list_area,
        );
        return;
    }
    let name_width = visible
        .iter()
        .map(|server| UnicodeWidthStr::width(server.name.as_str()))
        .max()
        .unwrap_or(0)
        .min(list_area.width.saturating_sub(18) as usize);
    let mut lines = Vec::new();
    let mut rows = Vec::new();
    let mut focus = 0;
    for server in &visible {
        let selected = state.mcp.selected.as_deref() == Some(server.name.as_str());
        if selected {
            focus = lines.len();
        }
        rows.push((lines.len(), server.name.clone()));
        let badge = format!("[{}]", state_label(state, server.state));
        let tools = match server.state {
            McpConnectionState::Connected => {
                format!("{} {}", server.tool_count.unwrap_or(0), t.tools_suffix)
            }
            _ => server.transport.label().to_string(),
        };
        lines.push(Line::from(vec![
            Span::styled(
                if selected { "› " } else { "  " }.to_string(),
                Style::default().fg(theme.accent.primary),
            ),
            Span::styled(
                pad(&badge, 15),
                Style::default().fg(state_color(state, server.state)),
            ),
            Span::styled(
                format!("{}  ", pad(&server.name, name_width.max(1))),
                Style::default()
                    .fg(theme.text.primary)
                    .add_modifier(if selected {
                        Modifier::BOLD
                    } else {
                        Modifier::empty()
                    }),
            ),
            Span::styled(tools, Style::default().fg(theme.text.muted)),
        ]));
    }
    let mut hits = Vec::new();
    crate::render::render_list_rows(frame, list_area, lines, &rows, focus, &theme, &mut hits);
    state.screen_rows = hits;
}

fn push_field(
    lines: &mut Vec<Line<'static>>,
    theme: &crate::theme::Theme,
    label: &str,
    value: &str,
) {
    lines.push(Line::from(vec![
        Span::styled(format!("{label}: "), Style::default().fg(theme.text.muted)),
        Span::styled(
            crate::render::text::sanitize_terminal_line(value),
            Style::default().fg(theme.text.primary),
        ),
    ]));
}

fn render_detail(frame: &mut Frame, area: Rect, state: &mut AppState, full: bool) {
    let t = &state.t().mcp_page;
    let theme = state.theme.clone();
    let detail_area = if area.width >= 80 && !full {
        Rect {
            x: area.x + area.width * 44 / 100 + 1,
            width: area.width - area.width * 44 / 100 - 1,
            ..area
        }
    } else {
        area
    };
    if detail_area.width == 0 || detail_area.height == 0 {
        return;
    }
    let Some(server) = selected_server(state).cloned() else {
        return;
    };
    let mut lines: Vec<Line<'static>> = Vec::new();
    lines.push(Line::from(vec![
        Span::styled(
            server.name.clone(),
            Style::default()
                .fg(theme.text.primary)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            format!("  [{}]", state_label(state, server.state)),
            Style::default().fg(state_color(state, server.state)),
        ),
    ]));
    push_field(
        &mut lines,
        &theme,
        t.detail_transport,
        server.transport.label(),
    );
    push_field(&mut lines, &theme, t.detail_endpoint, &server.endpoint);
    push_field(&mut lines, &theme, t.detail_source, &server.source);
    push_field(
        &mut lines,
        &theme,
        t.detail_enabled,
        if server.enabled { t.yes } else { t.no },
    );
    if let Some(protocol) = &server.protocol {
        push_field(&mut lines, &theme, t.detail_protocol, protocol);
    }
    push_field(
        &mut lines,
        &theme,
        t.detail_initialized,
        if server.initialized { t.yes } else { t.no },
    );
    if let Some(count) = server.tool_count {
        push_field(&mut lines, &theme, t.detail_tools, &count.to_string());
    }
    if let Some(at) = &server.connected_at {
        push_field(&mut lines, &theme, t.detail_connected_at, &stamp(at));
    }
    if let Some(at) = &server.last_attempt_at {
        push_field(&mut lines, &theme, t.detail_last_attempt, &stamp(at));
    }
    if let Some(at) = &server.updated_at {
        push_field(&mut lines, &theme, t.detail_updated_at, &stamp(at));
    }
    push_field(
        &mut lines,
        &theme,
        t.detail_generation,
        &server.generation.to_string(),
    );
    if let Some(error) = &server.last_error {
        lines.push(Line::default());
        lines.push(Line::from(Span::styled(
            format!("{}: ", t.detail_last_error),
            Style::default().fg(theme.status.error),
        )));
        for line in crate::render::text::wrap(error, detail_area.width as usize) {
            lines.push(Line::from(Span::styled(
                line,
                Style::default().fg(theme.status.error),
            )));
        }
    } else if server.state == McpConnectionState::NotConnected {
        lines.push(Line::default());
        lines.push(Line::from(Span::styled(
            t.not_connected_hint,
            Style::default().fg(theme.text.muted),
        )));
    }
    if state.mcp.tab == McpTab::Tools && server.state.is_connected() {
        lines.push(Line::default());
        lines.push(Line::from(Span::styled(
            t.tabs_tools.to_string(),
            Style::default()
                .fg(theme.text.muted)
                .add_modifier(Modifier::BOLD),
        )));
        if server.tools.is_empty() {
            lines.push(Line::from(Span::styled(
                t.tools_empty,
                Style::default().fg(theme.text.muted),
            )));
        }
        let skip = state.mcp.tool_scroll.min(server.tools.len());
        for tool in server.tools.iter().skip(skip) {
            lines.push(Line::default());
            lines.push(Line::from(vec![
                Span::styled(
                    tool.exposed_name.clone(),
                    Style::default()
                        .fg(theme.text.primary)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::styled(
                    format!(
                        "  {}: {}",
                        t.tool_risk,
                        if tool.risk == "network" {
                            t.risk_network
                        } else {
                            tool.risk.as_str()
                        }
                    ),
                    Style::default().fg(theme.text.muted),
                ),
                Span::styled(
                    format!(
                        "  {}: {}",
                        t.tool_admission,
                        admission_label(state, tool.admission)
                    ),
                    Style::default().fg(theme.text.muted),
                ),
            ]));
            for line in crate::render::text::wrap(&tool.description, detail_area.width as usize) {
                lines.push(Line::from(Span::styled(
                    line,
                    Style::default().fg(theme.text.muted),
                )));
            }
            if tool.truncated {
                lines.push(Line::from(Span::styled(
                    t.tool_schema_truncated,
                    Style::default().fg(theme.status.warning),
                )));
            }
        }
        if server.tools_truncated {
            lines.push(Line::from(Span::styled(
                t.tools_truncated,
                Style::default().fg(theme.status.warning),
            )));
        }
    }
    frame.render_widget(Paragraph::new(lines), detail_area);
}

fn render_confirm(frame: &mut Frame, area: Rect, state: &AppState) {
    let t = &state.t().mcp_page;
    let Some(name) = &state.mcp.confirm_remove else {
        return;
    };
    let height = 5.min(area.height);
    let width = area.width.min(72);
    let panel = Rect {
        x: area.x + (area.width - width) / 2,
        y: area.y + (area.height - height) / 2,
        width,
        height,
    };
    frame.render_widget(ratatui::widgets::Clear, panel);
    let block = ratatui::widgets::Block::default()
        .borders(ratatui::widgets::Borders::ALL)
        .title(t.confirm_remove_title);
    let inner = block.inner(panel);
    frame.render_widget(block, panel);
    let body = format!(
        "{}\n\n{}   ·   {}",
        t.confirm_remove_body.replace("{}", name),
        t.confirm_yes,
        t.confirm_no
    );
    frame.render_widget(Paragraph::new(body), inner);
}

fn render_form(frame: &mut Frame, area: Rect, state: &mut AppState) {
    let t = &state.t().mcp_page;
    let theme = state.theme.clone();
    let Some(form) = state.mcp.form.clone() else {
        return;
    };
    if area.width == 0 || area.height == 0 {
        return;
    }
    let muted = Style::default().fg(theme.text.muted);
    let focus_style = Style::default()
        .fg(theme.accent.primary)
        .add_modifier(Modifier::BOLD);
    let value_style = Style::default().fg(theme.text.primary);
    let mut lines = vec![Line::from(Span::styled(
        if form.editing.is_some() {
            t.form_edit_title
        } else {
            t.form_add_title
        }
        .to_string(),
        Style::default()
            .fg(theme.text.primary)
            .add_modifier(Modifier::BOLD),
    ))];
    if let Some(error) = &form.error {
        lines.push(Line::from(Span::styled(
            error.clone(),
            Style::default().fg(theme.status.error),
        )));
    }
    lines.push(Line::default());
    for field in form.fields() {
        let focused = form.focus == *field;
        let label = match field {
            McpField::Name => t.field_name,
            McpField::Transport => t.field_transport,
            McpField::Command => t.field_command,
            McpField::Args => t.field_args,
            McpField::Url => t.field_url,
            McpField::Env => t.field_env,
            McpField::Headers => t.field_headers,
            McpField::Bearer => t.field_bearer,
            McpField::Enabled => t.field_enabled,
        };
        let hint = match field {
            McpField::Name => t.hint_name,
            McpField::Transport => t.hint_transport,
            McpField::Command => t.hint_command,
            McpField::Args => t.hint_args,
            McpField::Url => t.hint_url,
            McpField::Env => t.hint_env,
            McpField::Headers => t.hint_headers,
            McpField::Bearer => t.hint_bearer,
            McpField::Enabled => t.hint_enabled,
        };
        if focused {
            lines.push(Line::from(Span::styled(
                format!("  {hint}"),
                Style::default().fg(theme.text.muted),
            )));
        }
        let label_style = if focused { focus_style } else { muted };
        let mut spans = vec![
            Span::styled(if focused { "› " } else { "  " }.to_string(), focus_style),
            Span::styled(format!("{label}: "), label_style),
        ];
        match field {
            McpField::Transport => {
                spans.push(Span::styled(
                    form.transport.label().to_string(),
                    value_style,
                ));
                spans.push(Span::styled(
                    if focused { "  ←→" } else { "" }.to_string(),
                    muted,
                ));
            }
            McpField::Enabled => {
                spans.push(Span::styled(
                    if form.enabled { t.yes } else { t.no }.to_string(),
                    value_style,
                ));
                let suffix = if focused {
                    format!("  {}", t.toggle_keys)
                } else {
                    String::new()
                };
                spans.push(Span::styled(suffix, muted));
            }
            _ => {
                let value = form.text(*field).cloned().unwrap_or_default();
                if focused {
                    let chars: Vec<char> = value.chars().collect();
                    let at = form.cursor.min(chars.len());
                    if at > 0 {
                        spans.push(Span::styled(
                            chars[..at].iter().collect::<String>(),
                            value_style,
                        ));
                    }
                    spans.push(Span::styled("▏".to_string(), focus_style));
                    if at < chars.len() {
                        spans.push(Span::styled(
                            chars[at..].iter().collect::<String>(),
                            value_style,
                        ));
                    }
                } else {
                    spans.push(Span::styled(
                        crate::render::text::sanitize_terminal_line(&value),
                        value_style,
                    ));
                }
            }
        }
        lines.push(Line::from(spans));
    }
    let scroll = if lines.len() > area.height as usize {
        form.field_index().saturating_sub(2)
    } else {
        0
    };
    let paragraph = Paragraph::new(lines).scroll((scroll as u16, 0));
    frame.render_widget(paragraph, area);
}

fn stamp(raw: &str) -> String {
    chrono::DateTime::parse_from_rfc3339(raw)
        .map(|value| {
            value
                .with_timezone(&chrono::Local)
                .format("%H:%M:%S")
                .to_string()
        })
        .unwrap_or_else(|_| raw.chars().take(19).collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::Boot;
    use crossterm::event::KeyModifiers;

    fn state() -> AppState {
        let mut state = AppState::new(
            crate::theme::Theme::no_color(),
            Boot {
                session_id: SessionId::new("s1"),
                user: "u".into(),
                version: "0".into(),
                show_welcome: false,
                draft_path: None,
                history_path: None,
                context_window: 0,
                locale: crate::i18n::Locale::Zh,
                untrusted_config: Vec::new(),
                model_notice: None,
                thinking: None,
            },
        );
        state.size = (100, 32);
        state.runtime_connected = true;
        state
    }

    fn server(name: &str, state: McpConnectionState) -> McpServerView {
        McpServerView {
            name: name.into(),
            transport: McpTransport::Stdio,
            endpoint: "npx".into(),
            source: "/tmp/config.toml".into(),
            spec: McpServerSpec {
                name: name.into(),
                transport: McpTransport::Stdio,
                command: Some("npx".into()),
                args: Vec::new(),
                env: Vec::new(),
                url: None,
                headers: Vec::new(),
                bearer_token_env: None,
                enabled: true,
            },
            enabled: true,
            state,
            protocol: None,
            initialized: false,
            tool_count: None,
            tools: Vec::new(),
            tools_truncated: false,
            last_error: None,
            last_attempt_at: None,
            connected_at: None,
            updated_at: None,
            generation: 0,
            retry_in_seconds: None,
        }
    }

    #[test]
    fn a_busy_form_submit_stays_visible_and_leaves_on_esc() {
        let mut state = state();
        state.status = leveler_client_protocol::RuntimeStatus::Busy;
        let mut form = McpForm::new();
        form.name = "fixture".into();
        form.command = "echo".into();
        state.mcp.form = Some(form);

        let effects = submit_form(&mut state);
        assert!(effects.is_empty(), "a busy mutation must not be sent");
        let form = state.mcp.form.as_ref().expect("the form stays open");
        assert!(!form.submitting, "a refused submit must not latch");
        assert!(
            form.error.as_deref().is_some_and(|error| !error.is_empty()),
            "the refusal must be visible on the form"
        );
        assert!(state.mcp.query.is_none());

        handle_form_key(&mut state, KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        assert!(state.mcp.form.is_none(), "Esc must still close the form");
    }

    #[test]
    fn a_timed_out_query_no_longer_blocks_a_retry() {
        let mut state = state();
        state.mcp.query = Some(CommandId::generate());
        state.mcp.query_started = Some(Instant::now() - QUERY_TIMEOUT - Duration::from_secs(1));
        let effects = reload(&mut state);
        assert!(
            matches!(
                effects.as_slice(),
                [Effect::Send(ClientCommand::ManageMcp { .. })]
            ),
            "a timed-out query must be replaceable: {effects:?}"
        );
    }

    #[test]
    fn the_first_list_selects_a_server_so_row_keys_can_act() {
        let mut state = state();
        let query = CommandId::generate();
        state.mcp.query = Some(query.clone());
        state.mcp.query_started = Some(Instant::now());
        state.mcp.pending = Some(McpIntent::List);
        let requester = state.session_id.clone();
        apply_managed(
            &mut state,
            &requester,
            &query,
            McpResponse::Snapshot {
                servers: vec![server("echo", McpConnectionState::NotConnected)],
            },
        );
        assert_eq!(state.mcp.selected.as_deref(), Some("echo"));
    }

    #[test]
    fn the_edit_form_is_prefilled_from_the_real_declaration() {
        let mut state = state();
        state.mcp.servers = vec![server("echo", McpConnectionState::Connected)];
        state.mcp.selected = Some("echo".into());
        begin_edit(&mut state);
        let form = state.mcp.form.as_ref().expect("form opened");
        assert_eq!(form.editing.as_deref(), Some("echo"));
        assert_eq!(form.name, "echo");
        assert_eq!(form.command, "npx");
        assert_eq!(form.transport, McpTransport::Stdio);
        assert!(form.enabled);
    }

    #[test]
    fn a_stale_answer_does_not_replace_a_newer_query() {
        let mut state = state();
        let query = CommandId::generate();
        state.mcp.query = Some(query.clone());
        state.mcp.query_started = Some(Instant::now());
        let stale = McpResponse::Snapshot {
            servers: vec![server("stale", McpConnectionState::Connected)],
        };
        let requester = state.session_id.clone();
        apply_managed(&mut state, &requester, &CommandId::generate(), stale);
        assert!(state.mcp.servers.is_empty());
        apply_managed(
            &mut state,
            &requester,
            &query,
            McpResponse::Snapshot {
                servers: vec![server("live", McpConnectionState::NotConnected)],
            },
        );
        assert_eq!(state.mcp.servers.len(), 1);
        assert!(!state.mcp.checking());
    }

    #[test]
    fn a_disconnected_runtime_never_claims_a_connection() {
        let mut state = state();
        state.runtime_connected = false;
        let effects = reload(&mut state);
        assert!(effects.is_empty());
        assert!(state.mcp.error.is_some());
    }

    #[test]
    fn a_busy_turn_refuses_mutations_but_allows_reads() {
        let mut state = state();
        state.status = leveler_client_protocol::RuntimeStatus::Busy;
        let effects = mutate(
            &mut state,
            McpRequest::Reconnect {
                server: "fs".into(),
            },
            McpIntent::Reconnect,
        );
        assert!(effects.is_empty());
        assert!(state.mcp.error.as_deref().is_some_and(|e| !e.is_empty()));
        let effects = reload(&mut state);
        assert_eq!(effects.len(), 1);
    }

    #[test]
    fn form_parses_quoted_arguments_and_name_refs() {
        assert_eq!(
            split_words(r#"-y --path "a b" c\ d"#),
            vec!["-y", "--path", "a b", "c d"]
        );
        let refs = parse_refs("FOO=FOO_ENV, BAR", true).unwrap();
        assert_eq!(refs.len(), 2);
        assert_eq!(refs[0].key, "FOO");
        assert_eq!(refs[0].source, "FOO_ENV");
        assert_eq!(refs[1].source, "BAR");
        assert!(parse_refs("Header", false).is_err());
    }

    #[test]
    fn the_form_submits_an_add_request_from_the_real_fields() {
        let mut state = state();
        state.mcp.form = Some(McpForm {
            editing: None,
            focus: McpField::Name,
            cursor: 0,
            name: "fs".into(),
            transport: McpTransport::Stdio,
            command: "npx".into(),
            args: "-y server".into(),
            url: String::new(),
            env: "GITHUB_TOKEN=GITHUB_TOKEN".into(),
            headers: String::new(),
            bearer: String::new(),
            enabled: true,
            submitting: false,
            error: None,
        });
        let effects = submit_form(&mut state);
        assert_eq!(effects.len(), 1);
        match &effects[0] {
            Effect::Send(ClientCommand::ManageMcp { request, .. }) => match request.as_ref() {
                McpRequest::Add { spec } => {
                    assert_eq!(spec.name, "fs");
                    assert_eq!(spec.args, vec!["-y", "server"]);
                    assert_eq!(spec.env[0].source, "GITHUB_TOKEN");
                }
                other => panic!("unexpected request: {other:?}"),
            },
            other => panic!("unexpected effect: {other:?}"),
        }
    }
}
