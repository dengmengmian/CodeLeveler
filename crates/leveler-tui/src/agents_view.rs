//! `/agents` — the agent registry inside the extensions workspace.
//!
//! The page is read-only. Creating and editing an agent stays a conversation
//! task. [`listing_note`] and [`detail_note`] are the text of the list and the
//! detail; a query this client did not send never reaches either.

use crossterm::event::{KeyCode, KeyEvent};
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};

use leveler_client_protocol::{
    ClientCommand, CommandId, UiAgentCapability, UiAgentDetail, UiAgentEntry, UiAgentProblem,
    UiAgentSource, UiAgentStatus,
};

use crate::action::Effect;
use crate::screen::Screen;
use crate::state::AppState;

/// The extensions-workspace Agents tab. Data arrives only from a query this
/// client sent; an empty page is "not loaded yet", not "there are no agents".
#[derive(Debug, Clone, Default)]
pub struct AgentsPage {
    pub entries: Vec<UiAgentEntry>,
    pub problems: Vec<UiAgentProblem>,
    pub selected: usize,
    pub query: String,
    pub searching: bool,
    pub detail: Option<UiAgentDetail>,
    pub error: Option<String>,
    pub loaded: bool,
    pub show_detail: bool,
}

/// Ask for the registry. A later answer is applied only when its query id
/// matches [`AppState::agents_list_query`].
pub fn request_list(state: &mut AppState) -> Vec<Effect> {
    if !state.runtime_connected {
        state.agents_page.error = Some(state.t().connection_lost.to_string());
        return Vec::new();
    }
    let query_id = CommandId::generate();
    state.agents_list_query = Some(query_id.clone());
    state.agents_page.error = None;
    vec![Effect::Send(ClientCommand::ListAgents {
        session_id: state.session_id.clone(),
        query_id: Some(query_id),
    })]
}

fn request_detail(state: &mut AppState, name: &str) -> Vec<Effect> {
    if !state.runtime_connected {
        state.agents_page.error = Some(state.t().connection_lost.to_string());
        return Vec::new();
    }
    let query_id = CommandId::generate();
    state.agent_detail_query = Some(query_id.clone());
    vec![Effect::Send(ClientCommand::GetAgent {
        session_id: state.session_id.clone(),
        name: name.to_string(),
        query_id: Some(query_id),
    })]
}

/// `/agents` and `/agents list`. A second bare command while the list is
/// showing closes the page, the same way `/skills` does.
pub fn open_list(state: &mut AppState) -> Vec<Effect> {
    if state.active_screen == Screen::Agents && !state.agents_page.show_detail {
        close(state);
        return Vec::new();
    }
    state.active_screen = Screen::Agents;
    state.screen_scroll = 0;
    state.detail_return = None;
    state.agents_page.show_detail = false;
    state.agents_page.searching = false;
    state.agents_page.query.clear();
    state.agents_page.selected = 0;
    request_list(state)
}

/// `/agents <name>` opens the detail. It does not toggle the page closed.
pub fn open_named(state: &mut AppState, name: &str) -> Vec<Effect> {
    state.active_screen = Screen::Agents;
    state.screen_scroll = 0;
    state.detail_return = None;
    state.agents_page.show_detail = true;
    state.agents_page.searching = false;
    state.agents_page.error = None;
    state.agents_page.detail = None;
    let mut effects = Vec::new();
    if !state.agents_page.loaded && state.agents_list_query.is_none() {
        effects.extend(request_list(state));
    }
    effects.extend(request_detail(state, name));
    effects
}

pub fn close(state: &mut AppState) {
    state.active_screen = Screen::Conversation;
    state.screen_scroll = 0;
    state.agents_page.show_detail = false;
    state.agents_page.searching = false;
}

/// One level back: search, then detail, then the conversation.
pub fn back(state: &mut AppState) -> Vec<Effect> {
    if state.agents_page.searching {
        state.agents_page.searching = false;
        return Vec::new();
    }
    if state.agents_page.show_detail {
        state.agents_page.show_detail = false;
        state.agents_page.error = None;
        state.screen_scroll = 0;
        return Vec::new();
    }
    close(state);
    Vec::new()
}

/// Store a list this client asked for. The transcript is not a second copy.
pub fn apply_list(state: &mut AppState, agents: Vec<UiAgentEntry>, problems: Vec<UiAgentProblem>) {
    state.agents_page.entries = agents;
    state.agents_page.problems = problems;
    state.agents_page.loaded = true;
    state.agents_page.error = None;
    clamp_selection(state);
}

/// Store one definition, or the reason it could not be loaded.
pub fn apply_detail(
    state: &mut AppState,
    name: &str,
    agent: Option<UiAgentDetail>,
    error: Option<String>,
) {
    state.agents_page.show_detail = true;
    state.screen_scroll = 0;
    match (agent, error) {
        (Some(detail), _) => {
            state.agents_page.detail = Some(detail);
            state.agents_page.error = None;
        }
        (None, Some(error)) => {
            state.agents_page.detail = None;
            state.agents_page.error = Some(error);
        }
        (None, None) => {
            state.agents_page.detail = None;
            state.agents_page.error =
                Some(state.t().workspace.agents_not_found.replace("{name}", name));
        }
    }
}

pub fn handle_key(state: &mut AppState, key: KeyEvent) -> Vec<Effect> {
    if state.agents_page.searching {
        match key.code {
            KeyCode::Esc | KeyCode::Enter => state.agents_page.searching = false,
            KeyCode::Backspace => {
                state.agents_page.query.pop();
                state.agents_page.selected = 0;
                clamp_selection(state);
            }
            // Letters, including j and k, belong to the query. Arrows move
            // the filtered list.
            KeyCode::Up => move_selection(state, -1),
            KeyCode::Down => move_selection(state, 1),
            KeyCode::Char(c) if !c.is_control() => {
                if state.agents_page.query.chars().count() < 120 {
                    state.agents_page.query.push(c);
                }
                state.agents_page.selected = 0;
                clamp_selection(state);
            }
            _ => {}
        }
        return Vec::new();
    }
    if state.agents_page.show_detail {
        match key.code {
            KeyCode::Esc => {
                back(state);
            }
            KeyCode::Up | KeyCode::Char('k') => {
                state.screen_scroll = state.screen_scroll.saturating_sub(1);
            }
            KeyCode::Down | KeyCode::Char('j') => {
                state.screen_scroll = state.screen_scroll.saturating_add(1);
            }
            KeyCode::PageUp => state.screen_scroll = state.screen_scroll.saturating_sub(10),
            KeyCode::PageDown => state.screen_scroll = state.screen_scroll.saturating_add(10),
            _ => {}
        }
        return Vec::new();
    }
    match key.code {
        KeyCode::Esc => return back(state),
        KeyCode::Up | KeyCode::Char('k') => move_selection(state, -1),
        KeyCode::Down | KeyCode::Char('j') => move_selection(state, 1),
        KeyCode::PageUp => move_selection(state, -10),
        KeyCode::PageDown => move_selection(state, 10),
        KeyCode::Enter => {
            let Some(index) = visible_indices(state)
                .get(state.agents_page.selected)
                .copied()
            else {
                return Vec::new();
            };
            let name = state.agents_page.entries[index].name.clone();
            state.agents_page.show_detail = true;
            state.screen_scroll = 0;
            if state
                .agents_page
                .detail
                .as_ref()
                .is_none_or(|detail| detail.entry.name != name)
            {
                state.agents_page.detail = None;
            }
            return request_detail(state, &name);
        }
        KeyCode::Char('s') | KeyCode::Char('/') => {
            state.agents_page.searching = true;
            state.screen_scroll = 0;
        }
        KeyCode::Char('r') => return request_list(state),
        _ => {}
    }
    Vec::new()
}

pub fn wheel(state: &mut AppState, notches: isize) {
    if state.agents_page.show_detail {
        let lines = (notches * 3).unsigned_abs();
        state.screen_scroll = if notches < 0 {
            state.screen_scroll.saturating_sub(lines)
        } else {
            state.screen_scroll.saturating_add(lines)
        };
        return;
    }
    move_selection(state, notches);
}

pub fn select_row(state: &mut AppState, key: &str) {
    let indices = visible_indices(state);
    if let Some(position) = indices
        .iter()
        .position(|index| agent_key(&state.agents_page.entries[*index]) == key)
    {
        state.agents_page.selected = position;
        return;
    }
    if let Ok(index) = key.parse::<usize>()
        && index < indices.len()
    {
        state.agents_page.selected = index;
    }
}

pub fn render_screen(frame: &mut Frame, area: Rect, state: &mut AppState) {
    let t = state.t();
    let hint = if state.agents_page.searching {
        t.workspace.agents_hint_search
    } else if state.agents_page.show_detail {
        t.workspace.agents_hint_detail
    } else {
        t.workspace.agents_hint_list
    };
    let page = crate::secondary::SecondaryPage {
        title: t.agents_title,
        status: crate::secondary::main_status_spans(state, area.width as usize),
        hint,
    };
    let layout = crate::secondary::page_layout(state, area, 0);
    crate::secondary::draw_header(frame, state, &layout, &page);
    crate::secondary::draw_footer(frame, state, &layout, &page);
    if state.agents_page.show_detail {
        render_detail(frame, layout.content, state);
    } else {
        render_list(frame, layout.content, state);
    }
}

fn visible_indices(state: &AppState) -> Vec<usize> {
    let query = state.agents_page.query.trim().to_lowercase();
    let mut indices: Vec<usize> = state
        .agents_page
        .entries
        .iter()
        .enumerate()
        .filter(|(_, entry)| {
            query.is_empty()
                || entry.name.to_lowercase().contains(&query)
                || entry
                    .description
                    .as_deref()
                    .unwrap_or("")
                    .to_lowercase()
                    .contains(&query)
        })
        .map(|(index, _)| index)
        .collect();
    // Paint order is the key order: built-in, then user, then project. Inside
    // a source the registry order is kept.
    indices.sort_by_key(|&index| (source_rank(state.agents_page.entries[index].source), index));
    indices
}

fn source_rank(source: UiAgentSource) -> u8 {
    match source {
        UiAgentSource::Builtin => 0,
        UiAgentSource::User => 1,
        UiAgentSource::Project => 2,
    }
}

fn clamp_selection(state: &mut AppState) {
    let len = visible_indices(state).len();
    if len == 0 {
        state.agents_page.selected = 0;
    } else if state.agents_page.selected >= len {
        state.agents_page.selected = len - 1;
    }
}

fn move_selection(state: &mut AppState, delta: isize) {
    let len = visible_indices(state).len();
    if len == 0 {
        state.agents_page.selected = 0;
        return;
    }
    let next = state.agents_page.selected as isize + delta;
    state.agents_page.selected = next.clamp(0, len as isize - 1) as usize;
}

fn render_list(frame: &mut Frame, area: Rect, state: &mut AppState) {
    let t = state.t();
    let theme = state.theme.clone();
    let width = area.width as usize;
    let mut lines: Vec<Line<'static>> = Vec::new();
    if state.agents_page.searching || !state.agents_page.query.is_empty() {
        lines.push(Line::from(vec![
            Span::styled(
                t.workspace.agents_search.to_string(),
                Style::default().fg(theme.text.muted),
            ),
            Span::styled(
                state.agents_page.query.clone(),
                Style::default()
                    .fg(theme.accent.primary)
                    .add_modifier(Modifier::BOLD),
            ),
        ]));
    }
    if let Some(error) = &state.agents_page.error {
        lines.extend(crate::render::list_state_lines(
            crate::render::ListStateBlock {
                kind: crate::render::ListStateKind::Error,
                message: &crate::render::sanitize_terminal_line(error),
                detail: None,
            },
            &theme,
            width,
        ));
    } else if !state.agents_page.loaded {
        lines.extend(crate::render::list_state_lines(
            crate::render::ListStateBlock {
                kind: crate::render::ListStateKind::Loading,
                message: t.workspace.agents_loading,
                detail: None,
            },
            &theme,
            width,
        ));
    }
    let indices = visible_indices(state);
    if state.agents_page.loaded && indices.is_empty() && state.agents_page.error.is_none() {
        lines.extend(crate::render::list_state_lines(
            crate::render::ListStateBlock {
                kind: if state.agents_page.query.trim().is_empty() {
                    crate::render::ListStateKind::Empty
                } else {
                    crate::render::ListStateKind::FilterEmpty
                },
                message: if state.agents_page.query.trim().is_empty() {
                    t.workspace.agents_empty
                } else {
                    t.workspace.agents_empty_filter
                },
                detail: None,
            },
            &theme,
            width,
        ));
    }
    let mut focus_line = None;
    let mut previous_source = None;
    let mut hits: Vec<crate::render::RowHitSpec> = Vec::new();
    for (position, index) in indices.iter().copied().enumerate() {
        let source = state.agents_page.entries[index].source;
        if previous_source != Some(source) {
            let count = indices
                .iter()
                .filter(|index| state.agents_page.entries[**index].source == source)
                .count();
            lines.push(crate::render::section_header_line(
                source_label(source, t),
                Some(count),
                None,
                &theme,
                width,
            ));
            previous_source = Some(source);
        }
        let entry = &state.agents_page.entries[index];
        let selected = position == state.agents_page.selected;
        if selected {
            focus_line = Some(lines.len());
        }
        let caption = crate::render::sanitize_terminal_line(&format!(
            "{}{}",
            capability_label(entry.capability, t),
            list_status(entry)
        ));
        let tone = match entry.status {
            UiAgentStatus::Available => crate::render::RowTone::Success,
            UiAgentStatus::Unavailable => crate::render::RowTone::Warning,
            UiAgentStatus::Invalid => crate::render::RowTone::Error,
        };
        let glyph = match entry.status {
            UiAgentStatus::Available => "●",
            UiAgentStatus::Unavailable => "!",
            UiAgentStatus::Invalid => "×",
        };
        let mut list_row =
            crate::render::ListRow::new(crate::render::sanitize_terminal_line(&entry.name))
                .status(glyph, tone)
                .meta(caption);
        if let Some(description) = entry.description.as_deref() {
            list_row = list_row
                .description(crate::render::sanitize_terminal_line(description))
                .wrap_description();
        }
        let key = agent_key(entry);
        let paint = crate::render::row_paint(state, &key, selected, true);
        let row_lines = crate::render::list_row_lines(&list_row, paint, &theme, width);
        hits.push(crate::render::RowHitSpec {
            line: lines.len(),
            height: row_lines.len() as u16,
            key,
        });
        lines.extend(row_lines);
    }
    if !state.agents_page.problems.is_empty() {
        lines.push(crate::render::section_header_line(
            t.agents_not_loaded,
            Some(state.agents_page.problems.len()),
            None,
            &theme,
            width,
        ));
        for problem in &state.agents_page.problems {
            let row =
                crate::render::ListRow::new(format!("{}: {}", problem.location, problem.error))
                    .status("!", crate::render::RowTone::Warning);
            lines.extend(crate::render::list_row_lines(
                &row,
                crate::render::RowPaint::default(),
                &theme,
                width,
            ));
        }
    }
    match focus_line {
        Some(focus) => crate::render::render_list_rows_with_spans(
            frame,
            area,
            lines,
            &hits,
            focus,
            &theme,
            &mut state.screen_rows,
        ),
        None => {
            state.screen_rows.clear();
            crate::render::render_scrolled(frame, area, state, lines);
        }
    }
}

fn agent_key(entry: &UiAgentEntry) -> String {
    format!("{:?}:{}", entry.source, entry.name)
}

fn render_detail(frame: &mut Frame, area: Rect, state: &mut AppState) {
    let theme = state.theme.clone();
    let t = state.t();
    let lines = if let Some(detail) = &state.agents_page.detail {
        detail_note(detail, t)
            .lines()
            .map(|line| {
                Line::from(Span::styled(
                    crate::render::sanitize_terminal_line(line),
                    Style::default().fg(theme.text.primary),
                ))
            })
            .collect()
    } else if let Some(error) = &state.agents_page.error {
        vec![Line::from(Span::styled(
            crate::render::sanitize_terminal_line(error),
            Style::default().fg(theme.status.error),
        ))]
    } else {
        vec![Line::from(Span::styled(
            t.workspace.agents_loading.to_string(),
            Style::default().fg(theme.text.muted),
        ))]
    };
    state.screen_rows.clear();
    crate::render::render_scrolled(frame, area, state, lines);
}

fn source(source: UiAgentSource) -> &'static str {
    match source {
        UiAgentSource::Project => "project",
        UiAgentSource::User => "user",
        UiAgentSource::Builtin => "builtin",
    }
}

/// Section heading. The same words the skills page uses, so the two lists do
/// not grow a second copy of "built-in".
fn source_label(source: UiAgentSource, t: &crate::i18n::UiText) -> &'static str {
    match source {
        UiAgentSource::Project => t.skills.scope_project,
        UiAgentSource::User => t.skills.scope_user,
        UiAgentSource::Builtin => t.skills.scope_builtin,
    }
}

fn capability_label(
    capability: Option<UiAgentCapability>,
    t: &crate::i18n::UiText,
) -> &'static str {
    match capability {
        Some(UiAgentCapability::ReadOnly) => t.agents_cap_read_only,
        Some(UiAgentCapability::Writer) => t.agents_cap_writer,
        Some(UiAgentCapability::ScopedWriter) => t.agents_cap_scoped,
        None => "?",
    }
}

fn capability(capability: Option<UiAgentCapability>) -> &'static str {
    match capability {
        Some(UiAgentCapability::ReadOnly) => "read_only",
        Some(UiAgentCapability::Writer) => "writer",
        Some(UiAgentCapability::ScopedWriter) => "scoped_writer",
        None => "?",
    }
}

fn status_suffix(entry: &UiAgentEntry) -> String {
    match entry.status {
        UiAgentStatus::Available if entry.harness_only => " · harness only".to_string(),
        UiAgentStatus::Available => String::new(),
        UiAgentStatus::Unavailable => format!(
            " · unavailable: {}",
            entry.reason.as_deref().unwrap_or("unknown")
        ),
        UiAgentStatus::Invalid => format!(
            " · INVALID: {}",
            entry.reason.as_deref().unwrap_or("unknown")
        ),
    }
}

/// The list row names a broken agent. A healthy harness-only role does not
/// carry that fact on the row; the detail and [`listing_note`] still do.
fn list_status(entry: &UiAgentEntry) -> String {
    match entry.status {
        UiAgentStatus::Available => String::new(),
        UiAgentStatus::Unavailable | UiAgentStatus::Invalid => status_suffix(entry),
    }
}

pub fn listing_note(
    agents: &[UiAgentEntry],
    problems: &[UiAgentProblem],
    t: &crate::i18n::UiText,
) -> String {
    let mut lines = vec![format!("{} ({})", t.agents_title, agents.len())];
    for entry in agents {
        let mut line = format!(
            "  {} — {} [{} · {}]{}",
            entry.name,
            entry.description.as_deref().unwrap_or("—"),
            capability(entry.capability),
            source(entry.source),
            status_suffix(entry),
        );
        for shadow in &entry.shadowed {
            line.push_str(&format!(" · shadows {}", source(shadow.source)));
        }
        lines.push(line);
    }
    if !problems.is_empty() {
        lines.push(format!("{} ({})", t.agents_not_loaded, problems.len()));
        for problem in problems {
            lines.push(format!("  {}: {}", problem.location, problem.error));
        }
    }
    lines.push(t.agents_hint.to_string());
    lines.join("\n")
}

pub fn detail_note(detail: &UiAgentDetail, t: &crate::i18n::UiText) -> String {
    let entry = &detail.entry;
    let mut lines = vec![format!(
        "Agent {} [{} · {}]{}",
        entry.name,
        capability(entry.capability),
        source(entry.source),
        status_suffix(entry)
    )];
    if let Some(description) = &entry.description {
        lines.push(format!("  {description}"));
    }
    if let Some(location) = &entry.location {
        lines.push(format!("  {}: {location}", t.agents_location));
    }
    if !entry.write_roots.is_empty() {
        lines.push(format!(
            "  {}: {}",
            t.agents_write_roots,
            entry.write_roots.join(", ")
        ));
    }
    lines.push(format!(
        "  {}: {}",
        t.agents_tools,
        entry
            .tools
            .as_ref()
            .map(|t| t.join(", "))
            .unwrap_or_else(|| t.agents_tools_all.to_string())
    ));
    lines.push(format!(
        "  {}: {}",
        t.agents_model,
        entry.model.as_deref().unwrap_or(t.agents_model_parent)
    ));
    if let Some(effort) = &entry.reasoning_effort {
        lines.push(format!("  {}: {effort}", t.agents_effort));
    }
    if !entry.skills.is_empty() {
        lines.push(format!(
            "  {}: {}",
            t.agents_skills,
            entry.skills.join(", ")
        ));
    }
    if let Some(rounds) = entry.max_rounds {
        lines.push(format!("  {}: {rounds}", t.agents_max_rounds));
    }
    if let Some(secs) = entry.max_duration_secs {
        lines.push(format!("  {}: {secs}s", t.agents_max_duration));
    }
    if let Some(fingerprint) = &entry.fingerprint {
        lines.push(format!("  {}: {fingerprint}", t.agents_fingerprint));
    }
    for shadow in &entry.shadowed {
        lines.push(format!(
            "  {} {}{}",
            t.agents_shadows,
            source(shadow.source),
            shadow
                .location
                .as_deref()
                .map(|l| format!(" at {l}"))
                .unwrap_or_default()
        ));
    }
    if let Some(instructions) = &detail.instructions {
        lines.push(format!("  {}:", t.agents_instructions));
        for line in instructions.lines() {
            lines.push(format!("    {line}"));
        }
    } else if entry.structural {
        lines.push(format!("  {}", t.agents_structural));
    }
    lines.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::i18n::Locale;

    fn entry() -> UiAgentEntry {
        UiAgentEntry {
            name: "code-reviewer".into(),
            description: Some("审查改动".into()),
            capability: Some(UiAgentCapability::ReadOnly),
            source: UiAgentSource::Builtin,
            status: UiAgentStatus::Available,
            harness_only: true,
            reason: None,
            shadowed: Vec::new(),
            structural: true,
            location: Some("/repo/.leveler/agents/x".into()),
            write_roots: Vec::new(),
            tools: None,
            model: None,
            reasoning_effort: None,
            skills: Vec::new(),
            max_rounds: None,
            max_duration_secs: None,
            fingerprint: None,
        }
    }

    /// §12 of the IA doc: chrome follows the locale, identifiers never do.
    /// This screen was the one place a Chinese session still read as English —
    /// every label on it ("hint", "location", "tools", "Not loaded") is Class A.
    #[test]
    fn the_listing_chrome_follows_the_locale_and_the_identifiers_do_not() {
        let agents = [entry()];
        let zh = listing_note(&agents, &[], Locale::Zh.text());
        let en = listing_note(&agents, &[], Locale::En.text());
        assert!(!zh.contains("hint:"), "zh kept English chrome:\n{zh}");
        assert!(en.contains("hint:"), "en lost its chrome:\n{en}");
        for id in ["code-reviewer", "read_only", "builtin"] {
            assert!(zh.contains(id), "zh rewrote {id}:\n{zh}");
            assert!(en.contains(id), "en rewrote {id}:\n{en}");
        }
    }

    /// The list is grouped like a directory: a source rule, then the name and
    /// a short capability tag, then the description indented under the name.
    /// Identifiers such as `read_only` stay in [`listing_note`], not on the row.
    #[test]
    fn the_list_groups_by_source_and_indents_the_description() {
        let mut project = entry();
        project.name = "worker".into();
        project.source = UiAgentSource::Project;
        project.capability = Some(UiAgentCapability::Writer);
        project.description = Some("写代码".into());
        project.harness_only = false;
        let mut user = entry();
        user.name = "notes".into();
        user.source = UiAgentSource::User;
        user.capability = Some(UiAgentCapability::ScopedWriter);
        user.description = Some("个人笔记".into());
        user.harness_only = false;
        let mut state = crate::state::AppState::new(
            crate::theme::Theme::no_color(),
            crate::state::Boot {
                session_id: leveler_client_protocol::SessionId::new("s1"),
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
        );
        state.agents_page.entries = vec![project, user, entry()];
        state.agents_page.loaded = true;

        let visible = visible_indices(&state);
        assert_eq!(
            visible
                .iter()
                .map(|index| state.agents_page.entries[*index].name.as_str())
                .collect::<Vec<_>>(),
            vec!["code-reviewer", "notes", "worker"],
            "j/k follow the painted source order"
        );

        let text = paint_list(&mut state, 64, 24);
        let builtin = text
            .lines()
            .find(|line| line.contains("内置"))
            .expect("builtin section");
        assert!(builtin.contains('─'), "section rule: {builtin:?}");
        let name = text
            .lines()
            .find(|line| line.contains("code-reviewer"))
            .expect("name row");
        assert!(name.contains("只读"), "capability tag: {name:?}");
        assert!(!name.contains("read_only"), "row leaked an id: {name:?}");
        assert!(
            !name.contains("builtin"),
            "source stays on the rule: {name:?}"
        );
        assert!(
            !name.contains("harness"),
            "harness stays off the row: {name:?}"
        );
        assert!(
            !name.contains("审查"),
            "description has its own lines: {name:?}"
        );
        let desc = text
            .lines()
            .find(|line| line.contains("审查改动"))
            .expect("description");
        assert_eq!(
            display_col(desc, "审查"),
            display_col(name, "code-reviewer"),
            "description aligns with the name:\nname={name:?}\ndesc={desc:?}"
        );
        let user_at = text.lines().position(|line| line.contains("用户")).unwrap();
        let project_at = text.lines().position(|line| line.contains("项目")).unwrap();
        let builtin_at = text.lines().position(|line| line.contains("内置")).unwrap();
        assert!(builtin_at < user_at && user_at < project_at, "{text}");

        state.locale = Locale::En;
        let english = paint_list(&mut state, 64, 24);
        let english_name = english
            .lines()
            .find(|line| line.contains("code-reviewer"))
            .expect("english name");
        assert!(english_name.contains("read-only"), "{english_name:?}");
        assert!(!english_name.contains("read_only"), "{english_name:?}");
        assert!(
            english
                .lines()
                .any(|line| line.contains("built-in") && line.contains('─')),
            "{english}"
        );
    }

    fn display_col(line: &str, needle: &str) -> usize {
        let byte = line.find(needle).expect(needle);
        unicode_width::UnicodeWidthStr::width(&line[..byte])
    }

    fn paint_list(state: &mut AppState, width: u16, height: u16) -> String {
        let mut term =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(width, height)).unwrap();
        term.draw(|frame| {
            render_list(
                frame,
                ratatui::layout::Rect::new(0, 0, width, height),
                state,
            );
        })
        .unwrap();
        let buf = term.backend().buffer();
        let mut out = String::new();
        for y in 0..height {
            let mut line = String::new();
            let mut skip = false;
            for x in 0..width {
                // A double-width glyph owns the next cell. That spacer is not
                // a character, so reading it inserts a gap into every CJK word.
                if skip {
                    skip = false;
                    continue;
                }
                let symbol = buf.cell((x, y)).map(|cell| cell.symbol()).unwrap_or(" ");
                if unicode_width::UnicodeWidthStr::width(symbol) == 2 {
                    skip = true;
                }
                line.push_str(symbol);
            }
            out.push_str(line.trim_end());
            out.push('\n');
        }
        out
    }

    #[test]
    fn a_description_sits_under_the_name_and_wraps() {
        let row = crate::render::ListRow::new("reviewer")
            .status("●", crate::render::RowTone::Success)
            .description("审查改动并且说明原因")
            .wrap_description();
        let lines = crate::render::list_row_lines(
            &row,
            crate::render::RowPaint::default(),
            &crate::theme::Theme::dark(),
            10,
        );
        let rows: Vec<String> = lines
            .iter()
            .map(|line| {
                line.spans
                    .iter()
                    .map(|span| span.content.as_ref())
                    .collect()
            })
            .collect();
        assert!(rows.len() > 2, "{rows:?}");
        for row in rows.iter().skip(1) {
            assert!(row.starts_with("    "), "{row:?}");
            assert!(
                unicode_width::UnicodeWidthStr::width(row.as_str()) <= 10,
                "{row:?}"
            );
        }
    }

    #[test]
    fn search_types_j_and_k_and_arrows_move_the_list() {
        let mut state = crate::state::AppState::new(
            crate::theme::Theme::no_color(),
            crate::state::Boot {
                session_id: leveler_client_protocol::SessionId::new("s1"),
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
        );
        let mut other = entry();
        other.name = "worker".into();
        state.agents_page.entries = vec![entry(), other];
        state.agents_page.loaded = true;
        state.agents_page.searching = true;
        handle_key(
            &mut state,
            crossterm::event::KeyEvent::new(KeyCode::Down, crossterm::event::KeyModifiers::empty()),
        );
        assert_eq!(state.agents_page.selected, 1);
        handle_key(
            &mut state,
            crossterm::event::KeyEvent::new(
                KeyCode::Char('j'),
                crossterm::event::KeyModifiers::empty(),
            ),
        );
        assert_eq!(state.agents_page.query, "j");
        assert_eq!(
            state.agents_page.selected, 0,
            "j is a letter in the query, not a move"
        );
    }

    #[test]
    fn the_detail_chrome_follows_the_locale_and_the_paths_do_not() {
        let detail = UiAgentDetail {
            entry: entry(),
            instructions: None,
        };
        let zh = detail_note(&detail, Locale::Zh.text());
        assert!(!zh.contains("location:"), "zh kept English chrome:\n{zh}");
        assert!(
            zh.contains("/repo/.leveler/agents/x"),
            "the path is data:\n{zh}"
        );
    }
}
