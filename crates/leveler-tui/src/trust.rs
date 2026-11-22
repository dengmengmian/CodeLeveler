//! `/trust` — project definitions the trust gate can see.
//!
//! The page is a human surface over the SAME store, discovery and approval
//! transaction the CLI uses (`leveler-definition-trust`). It never asks the
//! model, never treats display text as a state, and every mutation re-reads
//! the definition and compares the digest the person was shown before writing
//! a record. A file that changed under the decision is refused, not approved.

use std::collections::BTreeSet;
use std::path::PathBuf;

use crossterm::event::{KeyCode, KeyEvent};
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};

use leveler_definition_trust::{
    ApprovalRequest, ApprovalSurface, DefinitionCandidate, DefinitionIdentity, DefinitionTrust,
    DefinitionTrustView, DiscoveryRoots, approve, discover, read_snapshot, revoke_definition,
    unified_diff,
};

use crate::i18n::UiText;
use crate::screen::Screen;
use crate::state::AppState;
use crate::theme::Theme;

const PREVIEW_BYTES: usize = 256 * 1024;

/// Definition paths and bodies are untrusted terminal input. Keep tabs and
/// printable text; neutralize every other control character.
fn safe(text: &str) -> String {
    leveler_definition_trust::sanitize_line(text)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum TrustFilter {
    #[default]
    All,
    Untrusted,
    Trusted,
    Changed,
}

impl TrustFilter {
    fn next(self) -> Self {
        match self {
            Self::All => Self::Untrusted,
            Self::Untrusted => Self::Trusted,
            Self::Trusted => Self::Changed,
            Self::Changed => Self::All,
        }
    }

    fn label(self, t: &UiText) -> &'static str {
        match self {
            Self::All => t.trust.filter_all,
            Self::Untrusted => t.trust.filter_untrusted,
            Self::Trusted => t.trust.filter_trusted,
            Self::Changed => t.trust.filter_changed,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrustSection {
    Instructions,
    Skill,
    Agent,
    Other,
}

impl TrustSection {
    fn label(self, t: &UiText) -> &'static str {
        match self {
            Self::Instructions => t.trust.section_instructions,
            Self::Skill => t.trust.section_skills,
            Self::Agent => t.trust.section_agents,
            Self::Other => "",
        }
    }

    fn of(candidate: &DefinitionCandidate) -> Self {
        use leveler_definition_trust::DefinitionKind;
        match candidate.identity.kind {
            DefinitionKind::RootAgents
            | DefinitionKind::NestedAgents
            | DefinitionKind::Instructions
            | DefinitionKind::RuleFile => Self::Instructions,
            DefinitionKind::Skill => Self::Skill,
            DefinitionKind::Agent => Self::Agent,
        }
    }
}

#[derive(Debug, Clone)]
pub struct TrustRow {
    pub candidate: DefinitionCandidate,
    pub section: TrustSection,
    pub state: DefinitionTrust,
    pub unreadable: Option<String>,
    pub location: Option<PathBuf>,
    /// The persistent record's version, when a record exists (including a
    /// `changed` record whose bytes no longer match).
    pub approval_version: Option<u64>,
}

impl TrustRow {
    pub fn display(&self) -> &str {
        &self.candidate.identity.display
    }

    pub fn identity(&self) -> &DefinitionIdentity {
        &self.candidate.identity
    }

    fn digest(&self) -> Option<&str> {
        self.candidate.digest.as_deref()
    }

    pub fn is_pending(&self) -> bool {
        matches!(
            self.state,
            DefinitionTrust::Unapproved | DefinitionTrust::Changed { .. }
        )
    }

    fn is_revocable(&self) -> bool {
        matches!(
            self.state,
            DefinitionTrust::Trusted { .. } | DefinitionTrust::Changed { .. }
        )
    }
}

#[derive(Debug, Default)]
pub struct TrustState {
    pub rows: Vec<TrustRow>,
    pub selected: usize,
    pub filter: TrustFilter,
    pub detail_open: bool,
    pub show_diff: bool,
    /// Current-content preview and (when changed) the approved snapshot.
    pub detail_current: Option<String>,
    pub detail_approved: Option<String>,
    pub marked: BTreeSet<String>,
    pub notice: Option<String>,
    pub error: Option<String>,
    pub project: Option<PathBuf>,
    /// Store root override. `None` in production resolves the live Leveler
    /// home; tests use it so a page test never touches the real store.
    pub store_root: Option<PathBuf>,
}

/// Open `/trust`; a second invocation closes it.
pub fn open(state: &mut AppState) -> Vec<crate::action::Effect> {
    if state.active_screen == Screen::Trust {
        close(state);
        return Vec::new();
    }
    state.active_screen = Screen::Trust;
    state.screen_scroll = 0;
    state.trust.detail_open = false;
    state.trust.show_diff = false;
    state.trust.notice = None;
    state.trust.error = None;
    refresh(state);
    Vec::new()
}

pub fn close(state: &mut AppState) {
    state.active_screen = Screen::Conversation;
    state.screen_scroll = 0;
    state.trust.detail_open = false;
    state.trust.show_diff = false;
    state.trust.detail_current = None;
    state.trust.detail_approved = None;
}

/// One level back: detail → list → conversation.
pub fn back(state: &mut AppState) {
    if state.trust.detail_open {
        state.trust.detail_open = false;
        state.trust.show_diff = false;
        state.screen_scroll = 0;
        return;
    }
    close(state);
}

fn project_root(state: &AppState) -> PathBuf {
    state
        .repository
        .as_deref()
        .map(PathBuf::from)
        .or_else(|| std::env::current_dir().ok())
        .unwrap_or_default()
}

fn home_root(state: &AppState) -> PathBuf {
    state.trust.store_root.clone().unwrap_or_else(|| {
        leveler_core::LevelerHome::resolve(leveler_core::environment())
            .root()
            .to_path_buf()
    })
}

fn row_state(view: &DefinitionTrustView, candidate: &DefinitionCandidate) -> DefinitionTrust {
    match &candidate.digest {
        Some(digest) => view.check(&candidate.identity, digest),
        None => DefinitionTrust::Unapproved,
    }
}

/// Re-read discovery and the store. Preserves the selected identity and marks
/// that still exist.
pub fn refresh(state: &mut AppState) {
    let repo = project_root(state);
    state.trust.project = Some(repo.clone());
    let home = home_root(state);
    let view = DefinitionTrustView::load(&home, Some(&repo));
    let project_domain = leveler_definition_trust::project_domain(&repo);
    let roots = DiscoveryRoots::resolve(leveler_core::environment(), Some(repo));
    let previous = state
        .trust
        .rows
        .get(state.trust.selected)
        .map(|row| row.identity().id.clone());
    let mut rows: Vec<TrustRow> = discover(&roots)
        .into_iter()
        .map(|candidate| {
            let state = row_state(&view, &candidate);
            let unreadable = candidate.read_error.clone();
            let location = Some(candidate.primary_path.clone());
            let approval_version = match candidate.identity.scope {
                leveler_definition_trust::DefinitionScope::Project => view
                    .store()
                    .approval_record(&project_domain, &candidate.identity)
                    .map(|record| record.approval_version),
                leveler_definition_trust::DefinitionScope::User => view
                    .store()
                    .approval_record(leveler_definition_trust::USER_DOMAIN, &candidate.identity)
                    .map(|record| record.approval_version),
                leveler_definition_trust::DefinitionScope::Builtin => None,
            };
            TrustRow {
                section: TrustSection::of(&candidate),
                candidate,
                state,
                unreadable,
                location,
                approval_version,
            }
        })
        .collect();
    rows.sort_by(|left, right| {
        (left.section as u8, left.display()).cmp(&(right.section as u8, right.display()))
    });
    state.trust.rows = rows;
    let ids: BTreeSet<String> = state
        .trust
        .rows
        .iter()
        .map(|row| row.identity().id.clone())
        .collect();
    state.trust.marked.retain(|id| ids.contains(id));
    let filtered = filtered_len(state);
    state.trust.selected = previous
        .and_then(|id| {
            filtered_rows(state)
                .iter()
                .position(|row| row.identity().id == id)
        })
        .unwrap_or(0)
        .min(filtered.saturating_sub(1));
    if state.trust.detail_open {
        build_detail(state);
    }
}

fn matches_filter(row: &TrustRow, filter: TrustFilter) -> bool {
    match filter {
        TrustFilter::All => true,
        TrustFilter::Untrusted => {
            matches!(row.state, DefinitionTrust::Unapproved)
                || matches!(row.state, DefinitionTrust::StoreUnavailable { .. })
        }
        TrustFilter::Trusted => matches!(
            row.state,
            DefinitionTrust::Trusted { .. }
                | DefinitionTrust::SessionTrusted { .. }
                | DefinitionTrust::FullAdopted
        ),
        TrustFilter::Changed => matches!(row.state, DefinitionTrust::Changed { .. }),
    }
}

fn filtered_rows(state: &AppState) -> Vec<&TrustRow> {
    state
        .trust
        .rows
        .iter()
        .filter(|row| matches_filter(row, state.trust.filter))
        .collect()
}

fn filtered_len(state: &AppState) -> usize {
    filtered_rows(state).len()
}

fn selected_row(state: &AppState) -> Option<&TrustRow> {
    filtered_rows(state).get(state.trust.selected).copied()
}

/// Build the detail pane's exact text once, when it is opened or refreshed.
fn build_detail(state: &mut AppState) {
    let Some(row) = selected_row(state) else {
        state.trust.detail_current = None;
        state.trust.detail_approved = None;
        return;
    };
    let candidate = row.candidate.clone();
    let approved_digest = match &row.state {
        DefinitionTrust::Changed { approved_digest } => Some(approved_digest.clone()),
        DefinitionTrust::Trusted { .. } => row.digest().map(str::to_string),
        _ => None,
    };
    state.trust.detail_current = candidate.current_content().ok().map(|bytes| {
        let text = String::from_utf8_lossy(&bytes).into_owned();
        bounded_preview(&text)
    });
    state.trust.detail_approved = approved_digest.and_then(|digest| {
        read_snapshot(&home_root(state), &digest).map(|bytes| {
            let text = String::from_utf8_lossy(&bytes).into_owned();
            bounded_preview(&text)
        })
    });
}

fn bounded_preview(text: &str) -> String {
    if text.len() <= PREVIEW_BYTES {
        return text.to_string();
    }
    let mut end = PREVIEW_BYTES;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}\n… (preview truncated)", &text[..end])
}

/// Approve the selected row, or every marked row when `marked_only` is set.
fn approve_targets(state: &mut AppState, marked_only: bool) -> bool {
    let repo = project_root(state);
    let home = home_root(state);
    let targets: Vec<TrustRow> = if marked_only {
        state
            .trust
            .rows
            .iter()
            .filter(|row| state.trust.marked.contains(&row.identity().id) && row.is_pending())
            .cloned()
            .collect()
    } else {
        selected_row(state)
            .filter(|row| row.is_pending())
            .cloned()
            .into_iter()
            .collect()
    };
    if targets.is_empty() {
        state.trust.notice = Some(state.t().trust.nothing_to_approve.to_string());
        return false;
    }
    let mut requests: Vec<ApprovalRequest> = Vec::new();
    for row in &targets {
        let shown = row.digest();
        match row.candidate.approval_request() {
            Ok(request) if Some(request.digest()) == shown => requests.push(request),
            _ => {
                // Re-read moved away from the digest the person saw. Refuse the
                // whole transaction rather than writing an approval for bytes
                // nobody reviewed.
                state.trust.error = Some(format!(
                    "{} changed while the decision was being made; nothing was approved",
                    row.display()
                ));
                return false;
            }
        }
    }
    match approve(&home, &repo, &requests, ApprovalSurface::Tui) {
        Ok(report) => {
            state.trust.notice = Some(format!(
                "{} ({})",
                state.t().trust.approved,
                report.approved.len()
            ));
            state.trust.error = None;
            state.trust.marked.clear();
            refresh(state);
            true
        }
        Err(error) => {
            state.trust.error = Some(format!("{error}"));
            false
        }
    }
}

fn revoke_targets(state: &mut AppState, marked_only: bool) -> bool {
    let repo = project_root(state);
    let home = home_root(state);
    let identities: Vec<DefinitionIdentity> = if marked_only {
        state
            .trust
            .rows
            .iter()
            .filter(|row| state.trust.marked.contains(&row.identity().id) && row.is_revocable())
            .map(|row| row.identity().clone())
            .collect()
    } else {
        selected_row(state)
            .filter(|row| row.is_revocable())
            .map(|row| row.identity().clone())
            .into_iter()
            .collect()
    };
    if identities.is_empty() {
        state.trust.notice = Some(state.t().trust.nothing_to_revoke.to_string());
        return false;
    }
    let mut revoked = 0usize;
    for identity in identities {
        match revoke_definition(&home, &repo, &identity) {
            Ok(true) => revoked += 1,
            Ok(false) => {}
            Err(error) => {
                state.trust.error = Some(format!("{error}"));
                return false;
            }
        }
    }
    state.trust.notice = Some(format!("{} ({revoked})", state.t().trust.revoked));
    state.trust.error = None;
    refresh(state);
    true
}

/// Keys the page owns. Returns effects only for navigation the reducer owns.
pub fn handle_key(state: &mut AppState, key: KeyEvent) -> Vec<crate::action::Effect> {
    if state.trust.detail_open {
        match key.code {
            KeyCode::Esc => back(state),
            KeyCode::Up | KeyCode::Char('k') => {
                state.screen_scroll = state.screen_scroll.saturating_sub(1)
            }
            KeyCode::Down | KeyCode::Char('j') => {
                state.screen_scroll = state.screen_scroll.saturating_add(1)
            }
            KeyCode::PageUp => state.screen_scroll = state.screen_scroll.saturating_sub(10),
            KeyCode::PageDown => state.screen_scroll = state.screen_scroll.saturating_add(10),
            KeyCode::Char('d') => {
                // Re-read before comparing: the file may have moved since the
                // row was painted, and a stale digest would mislabel the diff.
                refresh(state);
                state.trust.show_diff = !state.trust.show_diff;
                state.screen_scroll = 0;
                build_detail(state);
            }
            KeyCode::Char('a') => {
                approve_targets(state, false);
                build_detail(state);
            }
            KeyCode::Char('r') => {
                revoke_targets(state, false);
                build_detail(state);
            }
            _ => {}
        }
        return Vec::new();
    }
    match key.code {
        KeyCode::Esc => back(state),
        KeyCode::Up | KeyCode::Char('k') => {
            state.trust.selected = state.trust.selected.saturating_sub(1);
        }
        KeyCode::Down | KeyCode::Char('j') => {
            let len = filtered_len(state);
            if len > 0 {
                state.trust.selected = (state.trust.selected + 1).min(len - 1);
            }
        }
        KeyCode::PageUp => {
            state.trust.selected = state.trust.selected.saturating_sub(10);
        }
        KeyCode::PageDown => {
            let len = filtered_len(state);
            state.trust.selected = (state.trust.selected + 10).min(len.saturating_sub(1));
        }
        KeyCode::Tab => {
            state.trust.filter = state.trust.filter.next();
            state.trust.selected = 0;
        }
        KeyCode::Char(' ') => {
            if let Some(row) = selected_row(state) {
                let id = row.identity().id.clone();
                if !state.trust.marked.remove(&id) {
                    state.trust.marked.insert(id);
                }
            }
        }
        KeyCode::Enter => {
            // Refresh first so the detail's digest/state describe the bytes it
            // is about to show.
            refresh(state);
            if selected_row(state).is_some() {
                state.trust.detail_open = true;
                state.trust.show_diff = false;
                state.screen_scroll = 0;
                build_detail(state);
            }
        }
        KeyCode::Char('a') => {
            approve_targets(state, false);
        }
        KeyCode::Char('A') => {
            approve_targets(state, true);
        }
        KeyCode::Char('r') => {
            revoke_targets(state, false);
        }
        KeyCode::Char('R') => {
            revoke_targets(state, true);
        }
        _ => {}
    }
    Vec::new()
}

pub fn wheel(state: &mut AppState, notches: isize) {
    if state.trust.detail_open {
        let lines = (notches * 3).unsigned_abs();
        state.screen_scroll = if notches < 0 {
            state.screen_scroll.saturating_sub(lines)
        } else {
            state.screen_scroll.saturating_add(lines)
        };
        return;
    }
    let len = filtered_len(state);
    if len == 0 {
        return;
    }
    let next = if notches < 0 {
        state.trust.selected.saturating_sub(notches.unsigned_abs())
    } else {
        (state.trust.selected + notches as usize).min(len - 1)
    };
    state.trust.selected = next;
}

pub fn select_row(state: &mut AppState, key: &str) {
    if let Ok(index) = key.parse::<usize>() {
        let len = filtered_len(state);
        if len > 0 {
            state.trust.selected = index.min(len - 1);
        }
    }
}

fn state_label(state: &DefinitionTrust, t: &UiText) -> &'static str {
    match state {
        DefinitionTrust::Trusted { .. }
        | DefinitionTrust::SessionTrusted { .. }
        | DefinitionTrust::FullAdopted
        | DefinitionTrust::Builtin => t.trust.state_trusted,
        DefinitionTrust::Unapproved => t.trust.state_unapproved,
        DefinitionTrust::Changed { .. } => t.trust.state_changed,
        DefinitionTrust::StoreUnavailable { .. } => t.trust.state_unavailable,
    }
}

fn state_marker(state: &DefinitionTrust) -> &'static str {
    match state {
        DefinitionTrust::Trusted { .. }
        | DefinitionTrust::SessionTrusted { .. }
        | DefinitionTrust::FullAdopted
        | DefinitionTrust::Builtin => "✓",
        DefinitionTrust::Unapproved => "○",
        DefinitionTrust::Changed { .. } => "!",
        DefinitionTrust::StoreUnavailable { .. } => "×",
    }
}

fn short_digest(digest: &str) -> String {
    digest
        .strip_prefix("sha256:")
        .unwrap_or(digest)
        .chars()
        .take(16)
        .collect()
}

pub fn render_screen(frame: &mut Frame, area: Rect, state: &mut AppState) {
    let t = state.t();
    let page = crate::secondary::SecondaryPage {
        title: t.trust.title,
        status: crate::secondary::main_status_spans(state, area.width as usize),
        hint: if state.trust.detail_open {
            t.trust.hint_detail
        } else {
            t.trust.hint_list
        },
    };
    let layout = crate::secondary::layout(area, 0);
    crate::secondary::draw_header(frame, state, &layout, &page);
    crate::secondary::draw_footer(frame, &layout, &page, &state.theme);
    if state.trust.detail_open {
        render_detail(frame, layout.content, state);
    } else {
        render_list(frame, layout.content, state);
    }
}

fn row_status_style(state: &DefinitionTrust, theme: &Theme) -> Style {
    match state {
        DefinitionTrust::Trusted { .. }
        | DefinitionTrust::SessionTrusted { .. }
        | DefinitionTrust::FullAdopted
        | DefinitionTrust::Builtin => Style::default().fg(theme.status.success),
        DefinitionTrust::Unapproved => Style::default().fg(theme.text.muted),
        DefinitionTrust::Changed { .. } => Style::default().fg(theme.status.warning),
        DefinitionTrust::StoreUnavailable { .. } => Style::default().fg(theme.status.error),
    }
}

fn render_list(frame: &mut Frame, area: Rect, state: &mut AppState) {
    let t = state.t();
    let theme = &state.theme;
    let project = state
        .trust
        .project
        .as_ref()
        .map(|path| safe(&path.display().to_string()))
        .unwrap_or_default();
    let mut lines: Vec<Line<'static>> = Vec::new();
    let mut row_lines: Vec<(usize, String)> = Vec::new();
    let mut focus: Option<usize> = None;
    lines.push(Line::from(vec![
        Span::styled(
            format!("{}: ", t.trust.project),
            Style::default().fg(theme.text.muted),
        ),
        Span::styled(project, Style::default().fg(theme.text.secondary)),
    ]));
    lines.push(Line::from(vec![
        Span::styled(
            format!("{}: ", t.trust.title),
            Style::default().fg(theme.text.muted),
        ),
        Span::styled(
            state.trust.filter.label(t),
            Style::default()
                .fg(theme.accent.primary)
                .add_modifier(Modifier::BOLD),
        ),
    ]));
    if state.mode == leveler_client_protocol::PermissionProfile::FullAccess {
        lines.push(Line::from(Span::styled(
            t.trust.full_mode_note,
            Style::default().fg(theme.status.warning),
        )));
    }
    if let Some(error) = &state.trust.error {
        lines.push(Line::from(Span::styled(
            format!("⚠ {error}"),
            Style::default().fg(theme.status.error),
        )));
    } else if let Some(notice) = &state.trust.notice {
        lines.push(Line::from(Span::styled(
            format!("✓ {notice}"),
            Style::default().fg(theme.status.success),
        )));
    }
    lines.push(Line::default());
    let rows = filtered_rows(state);
    if rows.is_empty() {
        lines.push(Line::from(Span::styled(
            t.trust.empty,
            Style::default().fg(theme.text.muted),
        )));
    }
    let mut last_section: Option<TrustSection> = None;
    for (index, row) in rows.iter().enumerate() {
        if last_section != Some(row.section) {
            if last_section.is_some() {
                lines.push(Line::default());
            }
            lines.push(Line::from(Span::styled(
                row.section.label(t),
                Style::default()
                    .fg(theme.accent.primary)
                    .add_modifier(Modifier::BOLD),
            )));
            last_section = Some(row.section);
        }
        let selected = index == state.trust.selected;
        let marker = if state.trust.marked.contains(&row.identity().id) {
            "●"
        } else {
            state_marker(&row.state)
        };
        let label = state_label(&row.state, t);
        let mut spans = vec![
            Span::styled(
                if selected { "› " } else { "  " },
                Style::default().fg(theme.accent.primary),
            ),
            Span::styled(format!("{marker} "), row_status_style(&row.state, theme)),
            Span::styled(
                safe(row.display()),
                if selected {
                    Style::default().add_modifier(Modifier::BOLD)
                } else {
                    Style::default()
                },
            ),
        ];
        if let Some(digest) = row.digest() {
            spans.push(Span::styled(
                format!("  {}", short_digest(digest)),
                Style::default().fg(theme.text.muted),
            ));
        }
        spans.push(Span::styled("  ", Style::default()));
        spans.push(Span::styled(label, row_status_style(&row.state, theme)));
        if row.unreadable.is_some() {
            spans.push(Span::styled(
                format!("  ({})", t.trust.state_unreadable),
                Style::default().fg(theme.status.warning),
            ));
        }
        row_lines.push((lines.len(), index.to_string()));
        if selected {
            focus = Some(lines.len());
        }
        lines.push(Line::from(spans));
    }
    match focus {
        Some(focus) => {
            let mut hits = Vec::new();
            crate::render::render_list_rows(
                frame, area, lines, &row_lines, focus, theme, &mut hits,
            );
            state.screen_rows = hits;
        }
        None => crate::render::render_scrolled(frame, area, state, lines),
    }
}

fn render_detail(frame: &mut Frame, area: Rect, state: &mut AppState) {
    let t = state.t();
    let theme = &state.theme;
    let Some(row) = selected_row(state).cloned() else {
        crate::render::render_scrolled(
            frame,
            area,
            state,
            vec![Line::from(Span::styled(
                t.trust.empty,
                Style::default().fg(theme.text.muted),
            ))],
        );
        return;
    };
    let mut lines: Vec<Line<'static>> = Vec::new();
    lines.push(Line::from(Span::styled(
        t.trust.detail_title,
        Style::default()
            .fg(theme.accent.primary)
            .add_modifier(Modifier::BOLD),
    )));
    lines.push(Line::default());
    lines.push(Line::from(vec![
        Span::styled("  ", Style::default()),
        Span::styled(
            safe(row.display()),
            Style::default().add_modifier(Modifier::BOLD),
        ),
    ]));
    if let Some(location) = &row.location {
        detail_line(
            &mut lines,
            t.trust.location,
            &safe(&location.display().to_string()),
            theme,
        );
    }
    let label = state_label(&row.state, t);
    detail_line(&mut lines, t.trust.overlay_state, label, theme);
    if let Some(digest) = row.digest() {
        detail_line(&mut lines, t.trust.digest, digest, theme);
    }
    if let Some(version) = row.approval_version {
        detail_line(&mut lines, t.trust.version, &version.to_string(), theme);
    }
    if let DefinitionTrust::Changed { approved_digest } = &row.state {
        detail_line(&mut lines, t.trust.approved_digest, approved_digest, theme);
    }
    if let Some(error) = &row.unreadable {
        detail_line(&mut lines, t.trust.unreadable, error, theme);
    }
    lines.push(Line::default());
    if state.trust.show_diff {
        lines.push(Line::from(Span::styled(
            t.trust.diff,
            Style::default()
                .fg(theme.accent.primary)
                .add_modifier(Modifier::BOLD),
        )));
        match (&state.trust.detail_approved, &state.trust.detail_current) {
            (Some(old), Some(new)) => match unified_diff(old, new, 2000) {
                Some(rows) => {
                    for row in rows {
                        lines.push(Line::from(Span::styled(
                            format!("  {row}"),
                            Style::default().fg(theme.text.secondary),
                        )));
                    }
                }
                None => lines.push(Line::from(Span::styled(
                    "  (the definitions are too large to diff here)",
                    Style::default().fg(theme.text.muted),
                ))),
            },
            (None, _) => lines.push(Line::from(Span::styled(
                t.trust.no_snapshot,
                Style::default().fg(theme.text.muted),
            ))),
            (_, None) => lines.push(Line::from(Span::styled(
                t.trust.unreadable,
                Style::default().fg(theme.status.error),
            ))),
        }
    } else {
        lines.push(Line::from(Span::styled(
            t.trust.content,
            Style::default()
                .fg(theme.accent.primary)
                .add_modifier(Modifier::BOLD),
        )));
        match &state.trust.detail_current {
            Some(text) => {
                for line in text.lines() {
                    lines.push(Line::from(Span::styled(
                        format!("  {}", safe(line)),
                        Style::default().fg(theme.text.secondary),
                    )));
                }
            }
            None => lines.push(Line::from(Span::styled(
                t.trust.unreadable,
                Style::default().fg(theme.status.error),
            ))),
        }
    }
    crate::render::render_scrolled(frame, area, state, lines);
}

fn detail_line(lines: &mut Vec<Line<'static>>, label: &str, value: &str, theme: &Theme) {
    lines.push(Line::from(vec![
        Span::styled("  ", Style::default()),
        Span::styled(format!("{label}: "), Style::default().fg(theme.text.muted)),
        Span::styled(value.to_string(), Style::default().fg(theme.text.secondary)),
    ]));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn filters_never_call_an_untrusted_definition_trusted() {
        let t = TrustFilter::All;
        assert_eq!(t.next(), TrustFilter::Untrusted);
        assert_eq!(TrustFilter::Untrusted.next(), TrustFilter::Trusted);
        assert_eq!(TrustFilter::Trusted.next(), TrustFilter::Changed);
        assert_eq!(TrustFilter::Changed.next(), TrustFilter::All);
    }

    fn app(repo: &std::path::Path, store: &std::path::Path) -> AppState {
        use leveler_client_protocol::SessionId;
        let mut state = AppState::new(
            crate::theme::Theme::no_color(),
            crate::state::Boot {
                session_id: SessionId::new("s1"),
                user: "u".into(),
                version: "0.1.0".into(),
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
        state.repository = Some(repo.display().to_string());
        state.trust.store_root = Some(store.to_path_buf());
        state
    }

    fn write_skill(repo: &std::path::Path, name: &str, body: &str) {
        let dir = repo.join(".leveler/skills").join(name);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("SKILL.md"),
            format!("---\nname: {name}\ndescription: Demo.\n---\n\n{body}\n"),
        )
        .unwrap();
    }

    #[test]
    fn the_page_lists_untrusted_definitions_and_approves_the_exact_bytes() {
        let root = tempfile::tempdir().unwrap();
        let repo = root.path().join("repo");
        let store = root.path().join("home");
        std::fs::create_dir_all(&repo).unwrap();
        std::fs::write(repo.join("AGENTS.md"), "ROOT RULES\n").unwrap();
        write_skill(&repo, "demo", "DEMO BODY");
        let mut state = app(&repo, &store);
        refresh(&mut state);
        assert!(
            state
                .trust
                .rows
                .iter()
                .any(|row| row.display() == "AGENTS.md" && row.is_pending())
        );
        let skill_index = filtered_rows(&state)
            .iter()
            .position(|row| row.display().contains(".leveler/skills/demo"))
            .expect("skill row");
        state.trust.selected = skill_index;
        handle_key(
            &mut state,
            KeyEvent::new(KeyCode::Char('a'), crossterm::event::KeyModifiers::NONE),
        );
        assert!(state.trust.error.is_none(), "{:?}", state.trust.error);
        assert!(
            state
                .trust
                .rows
                .iter()
                .find(|row| row.display().contains(".leveler/skills/demo"))
                .is_some_and(|row| matches!(row.state, DefinitionTrust::Trusted { .. }))
        );
        // The record is in the real store the CLI would read.
        let view = DefinitionTrustView::load(&store, Some(&repo));
        let candidate = discover(&DiscoveryRoots::resolve(
            leveler_core::environment(),
            Some(repo.clone()),
        ))
        .into_iter()
        .find(|candidate| candidate.identity.display.contains(".leveler/skills/demo"))
        .unwrap();
        assert!(
            view.check(&candidate.identity, candidate.digest.as_deref().unwrap())
                .admits()
        );
    }

    #[test]
    fn an_edit_after_approval_is_reported_as_changed_and_revocable() {
        let root = tempfile::tempdir().unwrap();
        let repo = root.path().join("repo");
        let store = root.path().join("home");
        std::fs::create_dir_all(&repo).unwrap();
        std::fs::write(repo.join("AGENTS.md"), "OLD\n").unwrap();
        let mut state = app(&repo, &store);
        refresh(&mut state);
        state.trust.selected = filtered_rows(&state)
            .iter()
            .position(|row| row.display() == "AGENTS.md")
            .unwrap();
        handle_key(
            &mut state,
            KeyEvent::new(KeyCode::Char('a'), crossterm::event::KeyModifiers::NONE),
        );
        std::fs::write(repo.join("AGENTS.md"), "NEW\n").unwrap();
        refresh(&mut state);
        let row = state
            .trust
            .rows
            .iter()
            .find(|row| row.display() == "AGENTS.md")
            .unwrap();
        assert!(matches!(row.state, DefinitionTrust::Changed { .. }));
        state.trust.detail_open = true;
        state.trust.show_diff = true;
        build_detail(&mut state);
        let old = state.trust.detail_approved.clone().unwrap();
        let new = state.trust.detail_current.clone().unwrap();
        let diff = unified_diff(&old, &new, 100).unwrap();
        assert!(diff.iter().any(|row| row == "- OLD"), "{diff:?}");
        assert!(diff.iter().any(|row| row == "+ NEW"), "{diff:?}");
        handle_key(
            &mut state,
            KeyEvent::new(KeyCode::Char('r'), crossterm::event::KeyModifiers::NONE),
        );
        refresh(&mut state);
        assert!(matches!(
            state
                .trust
                .rows
                .iter()
                .find(|row| row.display() == "AGENTS.md")
                .unwrap()
                .state,
            DefinitionTrust::Unapproved
        ));
    }

    #[test]
    fn marked_rows_are_approved_in_one_transaction() {
        let root = tempfile::tempdir().unwrap();
        let repo = root.path().join("repo");
        let store = root.path().join("home");
        std::fs::create_dir_all(&repo).unwrap();
        std::fs::write(repo.join("AGENTS.md"), "ROOT\n").unwrap();
        write_skill(&repo, "demo", "DEMO");
        let mut state = app(&repo, &store);
        refresh(&mut state);
        for row in &state.trust.rows {
            if row.is_pending() {
                state.trust.marked.insert(row.identity().id.clone());
            }
        }
        assert!(approve_targets(&mut state, true));
        assert!(state.trust.rows.iter().all(|row| !row.is_pending()));
        assert!(state.trust.marked.is_empty());
    }

    #[test]
    fn a_skill_changed_under_the_decision_is_not_approved() {
        let root = tempfile::tempdir().unwrap();
        let repo = root.path().join("repo");
        let store = root.path().join("home");
        std::fs::create_dir_all(&repo).unwrap();
        write_skill(&repo, "demo", "FIRST");
        let mut state = app(&repo, &store);
        refresh(&mut state);
        state.trust.selected = filtered_rows(&state)
            .iter()
            .position(|row| row.display().contains("demo"))
            .unwrap();
        // Change the bytes after the row was read but before the approval.
        write_skill(&repo, "demo", "SECOND");
        handle_key(
            &mut state,
            KeyEvent::new(KeyCode::Char('a'), crossterm::event::KeyModifiers::NONE),
        );
        assert!(state.trust.error.is_some(), "{:?}", state.trust.notice);
        assert!(
            state
                .trust
                .rows
                .iter()
                .find(|row| row.display().contains("demo"))
                .is_some_and(|row| row.is_pending())
        );
    }
}
