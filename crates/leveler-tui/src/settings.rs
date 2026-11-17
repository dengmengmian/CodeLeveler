//! `/settings` — one projection of the runtime's effective settings.
//!
//! The page is a reader over authorities that already exist: the runtime's
//! settings projection (configuration generation and providers), the current
//! session snapshot (model, thinking, permission) and the client's own theme.
//! It never writes configuration itself. Editing a row opens the same picker
//! or sends the same typed command the dedicated slash command uses, so
//! `/settings` cannot invent a second rule for a setting.
//!
//! Reload is the runtime's existing `/reload` path; a failure is shown as a
//! failure, and the page re-reads only when the runtime answers.

use std::time::{Duration, Instant};

use crossterm::event::{KeyCode, KeyEvent, MouseEvent, MouseEventKind};
use leveler_client_protocol::settings::{
    UiProviderSetting, UiSettingEffect, UiSettingScope, UiSettingSource, UiSettingsSnapshot,
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

const QUERY_TIMEOUT: Duration = Duration::from_secs(10);

const ROW_MODEL: &str = "model.session";
const ROW_DEFAULT_MODEL: &str = "model.default";
const ROW_THINKING: &str = "thinking.session";
const ROW_PERMISSION: &str = "permission.session";
const ROW_THEME: &str = "theme.client";
const ROW_CONTEXT: &str = "context.window";
const ROW_CONFIG_GLOBAL: &str = "config.global";
const ROW_CONFIG_PROJECT: &str = "config.project";
const ROW_CONFIG_RELOAD: &str = "config.reload";
const PROVIDER_PREFIX: &str = "provider:";

#[derive(Debug, Default)]
pub struct SettingsState {
    pub snapshot: Option<UiSettingsSnapshot>,
    pub query: Option<CommandId>,
    pub query_started: Option<Instant>,
    /// Stable selection by row id.
    pub selected: Option<String>,
    pub detail_open: bool,
    pub filter: String,
    pub filter_focus: bool,
    /// A reload the page asked for is in flight.
    pub pending_reload: bool,
    pub notice: Option<String>,
    pub error: Option<String>,
}

impl SettingsState {
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SettingsGroup {
    Session,
    Display,
    Providers,
    Context,
    Config,
}

impl SettingsGroup {
    fn label(self, state: &AppState) -> &'static str {
        let t = &state.t().settings_page;
        match self {
            Self::Session => t.section_session,
            Self::Display => t.section_display,
            Self::Providers => t.section_providers,
            Self::Context => t.section_context,
            Self::Config => t.section_config,
        }
    }
}

#[derive(Debug, Clone)]
pub struct SettingsRow {
    pub id: String,
    pub group: SettingsGroup,
    pub label: String,
    pub value: String,
    pub source: UiSettingSource,
    pub scope: UiSettingScope,
    pub editable: bool,
    pub effect: UiSettingEffect,
    pub requires_reload: bool,
    /// Extra facts for the detail pane (provider protocol, endpoints, …).
    pub detail: Vec<String>,
}

// ---------------------------------------------------------------------------
// Opening and requests
// ---------------------------------------------------------------------------

pub fn open(state: &mut AppState) -> Vec<Effect> {
    if state.active_screen == crate::screen::Screen::Settings {
        close(state);
        return Vec::new();
    }
    state.active_screen = crate::screen::Screen::Settings;
    state.screen_scroll = 0;
    state.detail_return = None;
    state.settings.detail_open = false;
    ensure_selection(state);
    reload(state)
}

pub fn close(state: &mut AppState) {
    state.active_screen = crate::screen::Screen::Conversation;
    state.screen_scroll = 0;
    state.settings.query = None;
    state.settings.query_started = None;
    state.settings.pending_reload = false;
    state.settings.filter.clear();
    state.settings.filter_focus = false;
}

/// Read the runtime's effective settings projection.
pub fn reload(state: &mut AppState) -> Vec<Effect> {
    if state.settings.query.is_some() {
        return Vec::new();
    }
    if !state.runtime_connected {
        state.settings.error = Some(state.t().settings_page.disconnected.to_string());
        return Vec::new();
    }
    let query_id = CommandId::generate();
    state.settings.query = Some(query_id.clone());
    state.settings.query_started = Some(Instant::now());
    state.settings.error = None;
    vec![Effect::Send(ClientCommand::QuerySettings {
        requester_session_id: state.session_id.clone(),
        query_id,
    })]
}

pub fn apply_loaded(
    state: &mut AppState,
    requester_session_id: &SessionId,
    query_id: &CommandId,
    snapshot: UiSettingsSnapshot,
) {
    if requester_session_id != &state.session_id {
        return;
    }
    if state.settings.query.as_ref() != Some(query_id) {
        return;
    }
    state.settings.query = None;
    state.settings.query_started = None;
    state.settings.snapshot = Some(snapshot);
    ensure_selection(state);
}

pub fn session_changed(state: &mut AppState) -> Vec<Effect> {
    state.settings.snapshot = None;
    state.settings.query = None;
    state.settings.query_started = None;
    state.settings.selected = None;
    ensure_selection(state);
    if state.active_screen == crate::screen::Screen::Settings {
        reload(state)
    } else {
        Vec::new()
    }
}

pub fn should_reload(event: &RuntimeEvent) -> bool {
    matches!(
        event,
        RuntimeEvent::ConfigReloaded { .. } | RuntimeEvent::SessionUpdated { .. }
    )
}

/// A `ConfigReloaded` answer settles a reload this page asked for.
pub fn observe_config_reloaded(state: &mut AppState, outcome: &str, error: Option<&str>) {
    if !state.settings.pending_reload {
        return;
    }
    state.settings.pending_reload = false;
    if outcome == "applied" || outcome == "unchanged" {
        state.settings.notice = Some(state.t().settings_page.notice_reloaded.to_string());
        state.settings.error = None;
    } else {
        state.settings.error = Some(format!(
            "{}: {}",
            state.t().settings_page.error_reload,
            error.unwrap_or(outcome)
        ));
    }
}

// ---------------------------------------------------------------------------
// Rows
// ---------------------------------------------------------------------------

pub fn rows(state: &AppState) -> Vec<SettingsRow> {
    let mut rows = Vec::new();
    rows.push(SettingsRow {
        id: ROW_MODEL.into(),
        group: SettingsGroup::Session,
        label: state.t().settings_page.row_model.to_string(),
        value: if state.model_label.is_empty() {
            state.t().settings_page.not_configured.to_string()
        } else {
            state.model_label.clone()
        },
        source: UiSettingSource::Session,
        scope: UiSettingScope::Session,
        editable: true,
        effect: UiSettingEffect::NextTurn,
        requires_reload: false,
        detail: Vec::new(),
    });
    let default_model = state.settings.snapshot.as_ref().and_then(|snapshot| {
        snapshot
            .resolved_default_model
            .as_ref()
            .map(ToString::to_string)
            .or_else(|| snapshot.default_model.clone())
    });
    rows.push(SettingsRow {
        id: ROW_DEFAULT_MODEL.into(),
        group: SettingsGroup::Session,
        label: state.t().settings_page.row_default_model.to_string(),
        value: default_model.unwrap_or_else(|| state.t().settings_page.not_configured.to_string()),
        source: UiSettingSource::GlobalConfig,
        scope: UiSettingScope::Global,
        editable: true,
        effect: UiSettingEffect::NextTurn,
        requires_reload: false,
        detail: Vec::new(),
    });
    let thinking = match &state.thinking {
        Some(thinking) => {
            let mut value = format!("{}", thinking.current);
            if thinking.effective != thinking.current {
                value.push_str(&format!(" → {}", thinking.effective));
            }
            value
        }
        None => state.t().settings_page.not_configured.to_string(),
    };
    let thinking_editable = state.thinking.as_ref().is_some_and(|thinking| {
        matches!(
            thinking.access,
            leveler_client_protocol::UiThinkingAccess::Adjustable
        )
    });
    let thinking_source = if state
        .thinking
        .as_ref()
        .is_some_and(|thinking| thinking.session_override.is_some())
    {
        UiSettingSource::Session
    } else {
        UiSettingSource::GlobalConfig
    };
    rows.push(SettingsRow {
        id: ROW_THINKING.into(),
        group: SettingsGroup::Session,
        label: state.t().settings_page.row_thinking.to_string(),
        value: thinking,
        source: thinking_source,
        scope: UiSettingScope::Session,
        editable: thinking_editable,
        effect: UiSettingEffect::NextTurn,
        requires_reload: false,
        detail: Vec::new(),
    });
    rows.push(SettingsRow {
        id: ROW_PERMISSION.into(),
        group: SettingsGroup::Session,
        label: state.t().settings_page.row_permission.to_string(),
        value: state.mode_label.clone(),
        source: UiSettingSource::Session,
        scope: UiSettingScope::Session,
        editable: true,
        effect: UiSettingEffect::Immediate,
        requires_reload: false,
        detail: Vec::new(),
    });
    rows.push(SettingsRow {
        id: ROW_THEME.into(),
        group: SettingsGroup::Display,
        label: state.t().settings_page.row_theme.to_string(),
        value: state.theme.id.as_str().to_string(),
        source: UiSettingSource::ClientLocal,
        scope: UiSettingScope::Client,
        editable: true,
        effect: UiSettingEffect::ClientOnly,
        requires_reload: false,
        detail: Vec::new(),
    });
    if let Some(snapshot) = &state.settings.snapshot {
        for provider in &snapshot.providers {
            rows.push(provider_row(state, provider));
        }
        let soft = snapshot
            .context_soft_percent
            .map(|percent| format!("{}%", percent))
            .unwrap_or_else(|| state.t().settings_page.not_configured.to_string());
        rows.push(SettingsRow {
            id: ROW_CONTEXT.into(),
            group: SettingsGroup::Context,
            label: state.t().settings_page.row_context.to_string(),
            value: format!(
                "{} · {} {}",
                state.context_window(),
                state.t().settings_page.context_soft,
                soft
            ),
            source: UiSettingSource::GlobalConfig,
            scope: UiSettingScope::Global,
            editable: false,
            effect: UiSettingEffect::NextTurn,
            requires_reload: false,
            detail: Vec::new(),
        });
        let mut sources = snapshot.sources.clone();
        sources.sort_by_key(|source| source.precedence);
        for source in &sources {
            let id = match source.kind.as_str() {
                "global" => ROW_CONFIG_GLOBAL,
                _ => ROW_CONFIG_PROJECT,
            };
            rows.push(SettingsRow {
                id: format!("{id}:{}", source.path),
                group: SettingsGroup::Config,
                label: if source.kind == "global" {
                    state.t().settings_page.row_config_global.to_string()
                } else {
                    state.t().settings_page.row_config_project.to_string()
                },
                value: source.path.clone(),
                source: if source.kind == "global" {
                    UiSettingSource::GlobalConfig
                } else {
                    UiSettingSource::ProjectConfig
                },
                scope: UiSettingScope::Global,
                editable: false,
                effect: UiSettingEffect::NextTurn,
                requires_reload: source.kind == "global",
                detail: vec![format!(
                    "{}: {} · {}: {}",
                    state.t().settings_page.detail_present,
                    source.present,
                    state.t().settings_page.detail_precedence,
                    source.precedence
                )],
            });
        }
        rows.push(SettingsRow {
            id: ROW_CONFIG_RELOAD.into(),
            group: SettingsGroup::Config,
            label: state.t().settings_page.row_config_reload.to_string(),
            value: String::new(),
            source: UiSettingSource::GlobalConfig,
            scope: UiSettingScope::Global,
            editable: true,
            effect: UiSettingEffect::Immediate,
            requires_reload: false,
            detail: Vec::new(),
        });
    }
    rows
}

fn provider_row(state: &AppState, provider: &UiProviderSetting) -> SettingsRow {
    let key = if provider.api_key_configured {
        state.t().settings_page.key_configured
    } else {
        state.t().settings_page.key_missing
    };
    SettingsRow {
        id: format!("{PROVIDER_PREFIX}{}", provider.id),
        group: SettingsGroup::Providers,
        label: provider.id.clone(),
        value: format!(
            "{} · {key} · {} {}",
            provider.protocol,
            provider.model_count,
            state.t().settings_page.model_count
        ),
        source: provider.source,
        scope: UiSettingScope::Global,
        editable: false,
        effect: UiSettingEffect::NextTurn,
        requires_reload: true,
        detail: std::iter::once(format!(
            "{}: {}",
            state.t().settings_page.detail_base_url,
            provider.base_url
        ))
        .chain(
            provider
                .api_key_env
                .as_ref()
                .map(|env| format!("{}: ${env}", state.t().settings_page.detail_credential)),
        )
        .collect(),
    }
}

fn visible_rows(state: &AppState) -> Vec<SettingsRow> {
    let filter = state.settings.filter.trim().to_lowercase();
    rows(state)
        .into_iter()
        .filter(|row| {
            filter.is_empty()
                || row.label.to_lowercase().contains(&filter)
                || row.value.to_lowercase().contains(&filter)
                || row.id.to_lowercase().contains(&filter)
        })
        .collect()
}

fn ensure_selection(state: &mut AppState) {
    let rows = visible_rows(state);
    let Some(first) = rows.first() else {
        state.settings.selected = None;
        return;
    };
    if let Some(selected) = &state.settings.selected
        && rows.iter().any(|row| &row.id == selected)
    {
        return;
    }
    state.settings.selected = Some(first.id.clone());
}

fn selected_row(state: &AppState) -> Option<SettingsRow> {
    let selected = state.settings.selected.as_deref()?;
    visible_rows(state)
        .into_iter()
        .find(|row| row.id == selected)
}

fn move_selection(state: &mut AppState, delta: isize) {
    let rows = visible_rows(state);
    if rows.is_empty() {
        state.settings.selected = None;
        return;
    }
    let current = state
        .settings
        .selected
        .as_deref()
        .and_then(|id| rows.iter().position(|row| row.id == id))
        .unwrap_or(0);
    let next = if delta < 0 {
        current.saturating_sub(delta.unsigned_abs())
    } else {
        (current + delta as usize).min(rows.len() - 1)
    };
    state.settings.selected = Some(rows[next].id.clone());
}

// ---------------------------------------------------------------------------
// Keys
// ---------------------------------------------------------------------------

pub fn handle_key(state: &mut AppState, key: KeyEvent) -> Vec<Effect> {
    if state.settings.filter_focus {
        return handle_filter_key(state, key);
    }
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
        KeyCode::Enter => activate(state),
        KeyCode::Char('/') => {
            state.settings.filter_focus = true;
            Vec::new()
        }
        KeyCode::Char('r') => reload(state),
        KeyCode::Char('R') => reload(state),
        _ => Vec::new(),
    }
}

fn handle_filter_key(state: &mut AppState, key: KeyEvent) -> Vec<Effect> {
    match key.code {
        KeyCode::Esc => {
            state.settings.filter_focus = false;
            if state.settings.filter.is_empty() {
                return Vec::new();
            }
            state.settings.filter.clear();
            ensure_selection(state);
            Vec::new()
        }
        KeyCode::Enter => {
            state.settings.filter_focus = false;
            Vec::new()
        }
        KeyCode::Backspace => {
            if let Some((index, _)) = state.settings.filter.char_indices().next_back() {
                state.settings.filter.truncate(index);
            }
            ensure_selection(state);
            Vec::new()
        }
        KeyCode::Char(ch)
            if !key
                .modifiers
                .intersects(crossterm::event::KeyModifiers::CONTROL) =>
        {
            state.settings.filter.push(ch);
            ensure_selection(state);
            Vec::new()
        }
        _ => Vec::new(),
    }
}

fn activate(state: &mut AppState) -> Vec<Effect> {
    let Some(row) = selected_row(state) else {
        return Vec::new();
    };
    match row.id.as_str() {
        ROW_MODEL => {
            crate::reducer::overlay_keys::open_model_picker(state);
            Vec::new()
        }
        ROW_DEFAULT_MODEL => {
            crate::reducer::overlay_keys::open_default_model_picker(state);
            Vec::new()
        }
        ROW_THINKING => {
            crate::reducer::overlay_keys::open_thinking_picker(state);
            Vec::new()
        }
        ROW_PERMISSION => {
            crate::reducer::overlay_keys::open_mode_picker(state);
            Vec::new()
        }
        ROW_THEME => {
            crate::reducer::overlay_keys::open_theme_picker(state);
            Vec::new()
        }
        ROW_CONTEXT => {
            crate::reducer::screen_nav::open_stats(state, crate::screen::StatsSection::Context)
        }
        ROW_CONFIG_RELOAD => {
            if state.settings.pending_reload {
                return Vec::new();
            }
            state.settings.pending_reload = true;
            vec![Effect::Send(ClientCommand::ReloadConfig {
                query_id: Some(CommandId::generate()),
            })]
        }
        id if id.starts_with(PROVIDER_PREFIX) => {
            state.settings.detail_open = !state.settings.detail_open;
            Vec::new()
        }
        _ => {
            state.settings.detail_open = !state.settings.detail_open;
            Vec::new()
        }
    }
}

pub fn handle_mouse(state: &mut AppState, mouse: MouseEvent) -> Vec<Effect> {
    match mouse.kind {
        MouseEventKind::ScrollUp => {
            move_selection(state, -1);
            Vec::new()
        }
        MouseEventKind::ScrollDown => {
            move_selection(state, 1);
            Vec::new()
        }
        MouseEventKind::Down(crossterm::event::MouseButton::Left) => {
            let point = (mouse.column, mouse.row).into();
            if let Some((_, key)) = state
                .screen_rows
                .iter()
                .find(|(rect, _)| rect.contains(point))
            {
                // A row click selects; Enter opens the editor or the detail,
                // matching the rest of the secondary pages.
                state.settings.selected = Some(key.clone());
                state.settings.detail_open = false;
            }
            Vec::new()
        }
        _ => Vec::new(),
    }
}

// ---------------------------------------------------------------------------
// Rendering
// ---------------------------------------------------------------------------

fn source_label(state: &AppState, source: UiSettingSource) -> &'static str {
    let t = &state.t().settings_page;
    match source {
        UiSettingSource::GlobalConfig => t.source_global,
        UiSettingSource::ProjectConfig => t.source_project,
        UiSettingSource::Environment => t.source_environment,
        UiSettingSource::Session => t.source_session,
        UiSettingSource::Default => t.source_default,
        UiSettingSource::ClientLocal => t.source_client,
        UiSettingSource::Unknown => t.source_unknown,
    }
}

fn scope_label(state: &AppState, scope: UiSettingScope) -> &'static str {
    let t = &state.t().settings_page;
    match scope {
        UiSettingScope::Global => t.scope_global,
        UiSettingScope::Session => t.scope_session,
        UiSettingScope::Client => t.scope_client,
    }
}

fn effect_label(state: &AppState, effect: UiSettingEffect) -> &'static str {
    let t = &state.t().settings_page;
    match effect {
        UiSettingEffect::Immediate => t.effect_immediate,
        UiSettingEffect::NextTurn => t.effect_next_turn,
        UiSettingEffect::Restart => t.effect_restart,
        UiSettingEffect::ClientOnly => t.effect_client,
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
    let t = &state.t().settings_page;
    let page = crate::secondary::SecondaryPage {
        title: t.title,
        status: crate::secondary::main_status_spans(state, area.width as usize),
        hint: t.footer,
    };
    let layout = crate::secondary::layout(area, 2);
    crate::secondary::draw_header(frame, state, &layout, &page);
    crate::secondary::draw_footer(frame, &layout, &page, &state.theme);
    render_filter(frame, layout.composer, state);
    let list_visible = layout.content.width >= 90 || !state.settings.detail_open;
    if list_visible {
        render_list(frame, layout.content, state);
    }
    if state.settings.detail_open || layout.content.width >= 90 {
        render_detail(frame, layout.content, state, !list_visible);
    }
    if !list_visible {
        state.screen_rows.clear();
    }
}

fn render_filter(frame: &mut Frame, area: Rect, state: &AppState) {
    if area.height == 0 || area.width == 0 {
        return;
    }
    let t = &state.t().settings_page;
    let line = format!("{}{}", t.filter_label, state.settings.filter);
    frame.render_widget(
        Paragraph::new(crate::render::text::sanitize_terminal_line(&line))
            .style(Style::default().fg(state.theme.text.muted)),
        area,
    );
    if state.settings.filter_focus {
        let cursor_text = crate::render::text::sanitize_terminal_line(&line);
        let x =
            UnicodeWidthStr::width(cursor_text.as_str()).min(area.width.saturating_sub(1) as usize);
        frame.set_cursor_position((area.x + x as u16, area.y));
    }
    let mut status = Vec::new();
    if state.settings.checking() || state.settings.pending_reload {
        status.push(t.pending.to_string());
    } else if state.settings.timed_out() {
        status.push(t.query_failed.to_string());
    }
    if let Some(notice) = &state.settings.notice {
        status.push(notice.clone());
    }
    if let Some(error) = &state.settings.error {
        status.push(error.clone());
    }
    if !status.is_empty() && area.height > 1 {
        frame.render_widget(
            Paragraph::new(crate::render::text::sanitize_terminal_line(
                &status.join("  ·  "),
            ))
            .style(Style::default().fg(if state.settings.error.is_some() {
                state.theme.status.error
            } else {
                state.theme.status.info
            })),
            Rect {
                y: area.y + 1,
                height: 1,
                ..area
            },
        );
    }
}

fn render_list(frame: &mut Frame, area: Rect, state: &mut AppState) {
    let t = &state.t().settings_page;
    let theme = state.theme.clone();
    if area.width == 0 || area.height == 0 {
        return;
    }
    let list_area = if area.width >= 90 {
        Rect {
            width: area.width * 48 / 100,
            ..area
        }
    } else {
        area
    };
    let rows = visible_rows(state);
    if rows.is_empty() {
        frame.render_widget(
            Paragraph::new(crate::render::text::sanitize_terminal_line(t.no_matches))
                .style(Style::default().fg(theme.text.muted)),
            list_area,
        );
        return;
    }
    let mut lines: Vec<Line<'static>> = Vec::new();
    let mut hits: Vec<(usize, String)> = Vec::new();
    let mut focus = 0;
    let mut last_group: Option<SettingsGroup> = None;
    for row in &rows {
        if last_group != Some(row.group) {
            if last_group.is_some() {
                lines.push(Line::default());
            }
            lines.push(Line::from(Span::styled(
                row.group.label(state).to_string(),
                Style::default()
                    .fg(theme.text.muted)
                    .add_modifier(Modifier::BOLD),
            )));
            last_group = Some(row.group);
        }
        let selected = state.settings.selected.as_deref() == Some(row.id.as_str());
        if selected {
            focus = lines.len();
        }
        hits.push((lines.len(), row.id.clone()));
        let value_width = (list_area.width as usize).saturating_sub(26).max(8);
        lines.push(Line::from(vec![
            Span::styled(
                if selected { "› " } else { "  " }.to_string(),
                Style::default().fg(theme.accent.primary),
            ),
            Span::styled(
                format!("{}  ", pad(&row.label, 16)),
                Style::default()
                    .fg(theme.text.primary)
                    .add_modifier(if selected {
                        Modifier::BOLD
                    } else {
                        Modifier::empty()
                    }),
            ),
            Span::styled(
                crate::render::text::truncate_display(&row.value, value_width),
                Style::default().fg(theme.text.muted),
            ),
        ]));
    }
    crate::render::render_list_rows(
        frame,
        list_area,
        lines,
        &hits,
        focus,
        &theme,
        &mut state.screen_rows,
    );
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

fn render_detail(frame: &mut Frame, area: Rect, state: &AppState, full: bool) {
    let t = &state.t().settings_page;
    let theme = state.theme.clone();
    let detail_area = if area.width >= 90 && !full {
        Rect {
            x: area.x + area.width * 48 / 100 + 1,
            width: area.width - area.width * 48 / 100 - 1,
            ..area
        }
    } else {
        area
    };
    if detail_area.width == 0 || detail_area.height == 0 {
        return;
    }
    let Some(row) = selected_row(state) else {
        return;
    };
    let mut lines = vec![
        Line::from(Span::styled(
            row.label.clone(),
            Style::default()
                .fg(theme.text.primary)
                .add_modifier(Modifier::BOLD),
        )),
        Line::from(Span::styled(
            crate::render::text::sanitize_terminal_line(&row.value),
            Style::default().fg(theme.text.primary),
        )),
        Line::default(),
    ];
    push_field(
        &mut lines,
        &theme,
        t.detail_source,
        source_label(state, row.source),
    );
    push_field(
        &mut lines,
        &theme,
        t.detail_scope,
        scope_label(state, row.scope),
    );
    push_field(
        &mut lines,
        &theme,
        t.detail_editable,
        if row.editable {
            t.editable
        } else {
            t.read_only
        },
    );
    push_field(
        &mut lines,
        &theme,
        t.detail_effect,
        effect_label(state, row.effect),
    );
    push_field(
        &mut lines,
        &theme,
        t.detail_reload,
        if row.requires_reload { t.yes } else { t.no },
    );
    for detail in &row.detail {
        for line in crate::render::text::wrap(detail, detail_area.width as usize) {
            lines.push(Line::from(Span::styled(
                line,
                Style::default().fg(theme.text.muted),
            )));
        }
    }
    if row.editable {
        lines.push(Line::default());
        lines.push(Line::from(Span::styled(
            t.press_enter_edit,
            Style::default().fg(theme.accent.primary),
        )));
    }
    if state.settings.snapshot.is_none() && state.settings.query.is_some() {
        lines.push(Line::from(Span::styled(
            t.loading,
            Style::default().fg(theme.text.muted),
        )));
    }
    frame.render_widget(Paragraph::new(lines), detail_area);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::Boot;

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
                context_window: 128_000,
                locale: crate::i18n::Locale::Zh,
                untrusted_config: Vec::new(),
                model_notice: None,
                thinking: None,
            },
        );
        state.size = (110, 34);
        state.runtime_connected = true;
        state.model_label = "deepseek/deepseek-v4-pro".into();
        state.mode_label = "Assisted".into();
        state
    }

    fn snapshot() -> UiSettingsSnapshot {
        UiSettingsSnapshot {
            generated_at: "2026-11-17T00:00:00Z".into(),
            config_version: 3,
            config_path: Some("/home/u/.leveler/config.toml".into()),
            project_config_dir: Some("/repo/configs".into()),
            default_model: Some("deepseek/deepseek-v4-pro".into()),
            resolved_default_model: Some(
                leveler_model::ModelRef::parse("deepseek/deepseek-v4-pro").unwrap(),
            ),
            thinking_default: Some(leveler_model::ThinkingLevel::High),
            theme_default: Some("dark".into()),
            context_soft_percent: Some(80),
            providers: vec![UiProviderSetting {
                id: "deepseek".into(),
                protocol: "openai_chat".into(),
                base_url: "https://api.deepseek.com".into(),
                api_key_env: Some("DEEPSEEK_API_KEY".into()),
                api_key_configured: true,
                model_count: 2,
                source: UiSettingSource::GlobalConfig,
            }],
            sources: vec![
                leveler_client_protocol::settings::UiConfigSource {
                    path: "/home/u/.leveler/config.toml".into(),
                    kind: "global".into(),
                    present: true,
                    precedence: 10,
                },
                leveler_client_protocol::settings::UiConfigSource {
                    path: "/repo/configs".into(),
                    kind: "project".into(),
                    present: true,
                    precedence: 20,
                },
            ],
        }
    }

    #[test]
    fn rows_cover_the_required_categories_and_carry_a_source() {
        let mut state = state();
        state.settings.snapshot = Some(snapshot());
        let rows = rows(&state);
        for id in [
            ROW_MODEL,
            ROW_DEFAULT_MODEL,
            ROW_THINKING,
            ROW_PERMISSION,
            ROW_THEME,
            ROW_CONTEXT,
            ROW_CONFIG_RELOAD,
        ] {
            assert!(rows.iter().any(|row| row.id == id), "missing row {id}");
        }
        assert!(rows.iter().any(|row| row.id == "provider:deepseek"));
        assert!(rows.iter().all(|row| !row.label.is_empty()));
    }

    #[test]
    fn a_stale_settings_answer_is_dropped() {
        let mut state = state();
        let query = CommandId::generate();
        state.settings.query = Some(query.clone());
        state.settings.query_started = Some(Instant::now());
        let requester = state.session_id.clone();
        apply_loaded(&mut state, &requester, &CommandId::generate(), snapshot());
        assert!(state.settings.snapshot.is_none());
        apply_loaded(&mut state, &requester, &query, snapshot());
        assert!(state.settings.snapshot.is_some());
    }

    #[test]
    fn a_reload_failure_is_never_a_silent_success() {
        let mut state = state();
        state.settings.pending_reload = true;
        observe_config_reloaded(&mut state, "failed", Some("bad TOML"));
        assert!(
            state
                .settings
                .error
                .as_deref()
                .unwrap()
                .contains("bad TOML")
        );
        assert!(state.settings.notice.is_none());
    }

    #[test]
    fn a_provider_row_never_renders_a_key_value() {
        let mut state = state();
        state.settings.snapshot = Some(snapshot());
        let row = rows(&state)
            .into_iter()
            .find(|row| row.id == "provider:deepseek")
            .unwrap();
        let text = format!("{} {:?}", row.value, row.detail);
        assert!(!text.contains("sk-"));
        assert!(text.contains("DEEPSEEK_API_KEY"));
    }
}
