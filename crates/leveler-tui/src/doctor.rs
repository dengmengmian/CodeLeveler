//! `/doctor` — structured diagnostics for the live client and its environment.
//!
//! Two producers fill one report:
//!
//! - this module, from facts the client itself observed (runtime connection,
//!   build, session validity, recovery gate, permission profile, terminal),
//! - the runtime, through [`ClientCommand::QueryDiagnostics`], which runs the
//!   same check core as `leveler doctor` (configuration, providers, models,
//!   MCP configuration, sandbox capability, agent registry).
//!
//! Every row is a [`DiagnosticFinding`] with a stable id and a typed status.
//! Nothing here infers a state from log text or model prose. UNKNOWN and
//! NOT_CHECKED are never painted as PASS, and the page never runs a fix: the
//! action line names an existing operation the user can perform.

use std::time::{Duration, Instant};

use crossterm::event::{KeyCode, KeyEvent, MouseEvent, MouseEventKind};
use leveler_client_protocol::{
    ClientCommand, CommandId, DiagnosticCategory, DiagnosticFinding, DiagnosticReport,
    DiagnosticSeverity, DiagnosticStatus, RuntimeEvent, SessionId, action_id, finding_id,
};
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use unicode_width::UnicodeWidthStr;

use crate::action::Effect;
use crate::state::AppState;

/// A diagnostics answer that never arrives must not look like "still
/// checking" forever. After this long the page says the request failed; the
/// answer (if it lands) is still applied, because it is a fact.
const QUERY_TIMEOUT: Duration = Duration::from_secs(10);

/// The eight areas shown in the overview, in reading order.
const CATEGORIES: [DiagnosticCategory; 8] = [
    DiagnosticCategory::Runtime,
    DiagnosticCategory::Model,
    DiagnosticCategory::Mcp,
    DiagnosticCategory::Permission,
    DiagnosticCategory::Sandbox,
    DiagnosticCategory::Terminal,
    DiagnosticCategory::Configuration,
    DiagnosticCategory::Recovery,
];

#[derive(Debug, Default)]
pub struct DoctorState {
    /// The last authoritative answer from the runtime.
    pub report: Option<DiagnosticReport>,
    /// The one query this page accepts the answer for.
    pub query: Option<CommandId>,
    pub query_started: Option<Instant>,
    /// Stable selection by finding key (`id` or `id:subject`).
    pub selected: Option<String>,
    /// The evidence/action panel for the selected finding is open.
    pub detail_open: bool,
}

impl DoctorState {
    pub fn checking(&self) -> bool {
        self.query.is_some()
            && self
                .query_started
                .is_some_and(|at| at.elapsed() < QUERY_TIMEOUT)
    }

    pub fn timed_out(&self) -> bool {
        self.query.is_some()
            && self
                .query_started
                .is_some_and(|at| at.elapsed() >= QUERY_TIMEOUT)
    }
}

/// Open the page, or close it when it is already open.
pub fn open(state: &mut AppState) -> Vec<Effect> {
    if state.active_screen == crate::screen::Screen::Doctor {
        close(state);
        return Vec::new();
    }
    state.active_screen = crate::screen::Screen::Doctor;
    state.screen_scroll = 0;
    state.detail_return = None;
    state.doctor.detail_open = true;
    ensure_selection(state);
    reload(state)
}

pub fn close(state: &mut AppState) {
    state.active_screen = crate::screen::Screen::Conversation;
    state.screen_scroll = 0;
    state.doctor.query = None;
    state.doctor.query_started = None;
}

/// Ask the runtime for the shared check core. Local findings are always
/// present; a disconnected runtime only means the runtime-owned checks are not
/// re-read.
pub fn reload(state: &mut AppState) -> Vec<Effect> {
    if !state.runtime_connected {
        state.doctor.query = None;
        state.doctor.query_started = None;
        return Vec::new();
    }
    let query = CommandId::generate();
    state.doctor.query = Some(query.clone());
    state.doctor.query_started = Some(Instant::now());
    ensure_selection(state);
    vec![Effect::Send(ClientCommand::QueryDiagnostics {
        requester_session_id: state.session_id.clone(),
        query_id: query,
    })]
}

/// Accept only the answer belonging to this client and this query.
pub fn apply_loaded(
    state: &mut AppState,
    requester_session_id: &SessionId,
    query_id: &CommandId,
    report: DiagnosticReport,
) {
    if requester_session_id != &state.session_id {
        return;
    }
    if state.doctor.query.as_ref() != Some(query_id) {
        return;
    }
    state.doctor.query = None;
    state.doctor.query_started = None;
    state.doctor.report = Some(report);
    ensure_selection(state);
}

/// A session switch invalidates the runtime-owned findings' attribution. The
/// local facts follow the new session immediately; the shared checks are
/// re-read if the page is open.
pub fn session_changed(state: &mut AppState) -> Vec<Effect> {
    state.doctor.report = None;
    state.doctor.query = None;
    state.doctor.query_started = None;
    state.doctor.selected = None;
    ensure_selection(state);
    if state.active_screen == crate::screen::Screen::Doctor {
        reload(state)
    } else {
        Vec::new()
    }
}

/// Whether an event should make an open doctor page re-read the shared
/// checks. Deliberately narrow: a diagnostics run is not a per-event action.
pub fn should_reload(event: &RuntimeEvent) -> bool {
    matches!(event, RuntimeEvent::RuntimeReconnected { .. })
}

// ---------------------------------------------------------------------------
// Findings
// ---------------------------------------------------------------------------

fn now() -> String {
    leveler_core::now().to_rfc3339()
}

fn local_finding(
    id: &str,
    subject: Option<String>,
    category: DiagnosticCategory,
    status: DiagnosticStatus,
    evidence: Vec<String>,
    action: Option<&str>,
) -> DiagnosticFinding {
    let severity = match status {
        DiagnosticStatus::Fail => DiagnosticSeverity::Error,
        DiagnosticStatus::Warn => DiagnosticSeverity::Warning,
        _ => DiagnosticSeverity::Info,
    };
    DiagnosticFinding {
        id: id.to_string(),
        subject,
        category,
        severity,
        status,
        evidence,
        action: action.map(str::to_string),
        checked_at: now(),
    }
}

/// Facts this client owns: no runtime round trip, no environment probe beyond
/// the terminal it is already running in.
pub fn local_findings(state: &AppState) -> Vec<DiagnosticFinding> {
    let mut out = Vec::new();

    out.push(local_finding(
        finding_id::RUNTIME_CONNECTION,
        None,
        DiagnosticCategory::Runtime,
        if state.runtime_connected {
            DiagnosticStatus::Pass
        } else {
            // The client observed the stream close. That is a confirmed
            // failure of the connection, not an unknown.
            DiagnosticStatus::Fail
        },
        vec![if state.runtime_connected {
            "runtime event stream is connected".into()
        } else {
            "runtime event stream is not connected".into()
        }],
        None,
    ));

    out.push(match &state.runtime_build {
        Some(build) if build.is_known() => local_finding(
            finding_id::RUNTIME_BUILD,
            None,
            DiagnosticCategory::Runtime,
            if state.runtime_version_mismatch {
                DiagnosticStatus::Warn
            } else {
                DiagnosticStatus::Pass
            },
            vec![format!(
                "serving build {} ({})",
                build.version, build.revision
            )],
            state
                .runtime_version_mismatch
                .then_some(action_id::RESTART_TO_MATCH),
        ),
        _ => local_finding(
            finding_id::RUNTIME_BUILD,
            None,
            DiagnosticCategory::Runtime,
            DiagnosticStatus::Unknown,
            vec!["the runtime has not reported a known build identity".into()],
            None,
        ),
    });

    let snapshot_seen = state.snapshot_version.is_some();
    out.push(local_finding(
        finding_id::RUNTIME_SESSION,
        None,
        DiagnosticCategory::Runtime,
        if !snapshot_seen {
            DiagnosticStatus::Unknown
        } else if state.session_deleted {
            DiagnosticStatus::Fail
        } else {
            DiagnosticStatus::Pass
        },
        if !snapshot_seen {
            vec![
                format!("session {}", state.session_id),
                "no session snapshot has been observed yet".into(),
            ]
        } else {
            vec![
                format!("session {}", state.session_id),
                if state.session_deleted {
                    "the runtime's session list no longer contains this session".into()
                } else {
                    "session is present on the runtime".into()
                },
            ]
        },
        None,
    ));

    let interrupted = state.interrupted_tool_calls.len();
    out.push(local_finding(
        finding_id::RUNTIME_RECOVERY,
        None,
        DiagnosticCategory::Recovery,
        if !snapshot_seen {
            DiagnosticStatus::Unknown
        } else if interrupted == 0 {
            DiagnosticStatus::Pass
        } else {
            DiagnosticStatus::Warn
        },
        if !snapshot_seen {
            vec!["no session snapshot has been observed, so the crash window is unknown".into()]
        } else if interrupted == 0 {
            vec!["no crash window is pending for this session".into()]
        } else {
            vec![format!(
                "{interrupted} interrupted tool call(s) await acknowledgement; the next turn is gated"
            )]
        },
        (interrupted > 0).then_some(action_id::ACKNOWLEDGE_RECOVERY),
    ));

    out.push(local_finding(
        finding_id::PERMISSION_PROFILE,
        None,
        DiagnosticCategory::Permission,
        if !snapshot_seen {
            DiagnosticStatus::Unknown
        } else {
            DiagnosticStatus::Pass
        },
        if !snapshot_seen {
            vec!["the runtime has not reported this session's permission profile yet".into()]
        } else {
            vec![
                format!("effective profile: {}", state.mode_label),
                "explicit run-level restrictions, when present, are not revoked by a profile"
                    .into(),
            ]
        },
        None,
    ));

    let terminal = crate::terminal_diagnostics::TerminalDiagnostics::detect();
    out.push(local_finding(
        finding_id::TERMINAL_CAPABILITY,
        None,
        DiagnosticCategory::Terminal,
        DiagnosticStatus::Pass,
        terminal
            .describe_localized(state.locale)
            .lines()
            .map(str::to_string)
            .collect(),
        None,
    ));

    out
}

/// The runtime's findings and the client's own, deduplicated by stable key.
/// The client owns the facts it observes directly; a runtime answer for the
/// same key would be stale by definition and is ignored.
pub fn all_findings(state: &AppState) -> Vec<DiagnosticFinding> {
    let local = local_findings(state);
    let mut findings = local.clone();
    if let Some(report) = &state.doctor.report {
        for finding in &report.findings {
            if local.iter().any(|owned| owned.key() == finding.key()) {
                continue;
            }
            findings.push(finding.clone());
        }
    }
    // Problems first, as Grok Build's doctor report does. The rest keep a
    // stable category/key order so the same report always reads the same way.
    findings.sort_by(|a, b| {
        status_rank(b.status)
            .cmp(&status_rank(a.status))
            .then_with(|| category_order(a.category).cmp(&category_order(b.category)))
            .then_with(|| a.key().cmp(&b.key()))
    });
    findings
}

fn category_order(category: DiagnosticCategory) -> usize {
    CATEGORIES
        .iter()
        .position(|candidate| *candidate == category)
        .unwrap_or(CATEGORIES.len())
}

/// The worst status among a category's findings. "Worst" is a safety ordering,
/// not a severity merge: a category cannot claim PASS while one of its checks
/// was never run.
pub fn category_status(state: &AppState, category: DiagnosticCategory) -> Option<DiagnosticStatus> {
    let findings = all_findings(state);
    category_status_of(&findings, category)
}

fn category_status_of(
    findings: &[DiagnosticFinding],
    category: DiagnosticCategory,
) -> Option<DiagnosticStatus> {
    findings
        .iter()
        .filter(|finding| finding.category == category)
        .map(|finding| finding.status)
        .max_by_key(|status| status_rank(*status))
}

fn status_rank(status: DiagnosticStatus) -> u8 {
    match status {
        DiagnosticStatus::NotApplicable => 0,
        DiagnosticStatus::Pass => 1,
        DiagnosticStatus::NotChecked => 2,
        DiagnosticStatus::Unknown => 3,
        DiagnosticStatus::Warn => 4,
        DiagnosticStatus::Fail => 5,
    }
}

fn status_style(state: &AppState, status: DiagnosticStatus) -> Style {
    let color = match status {
        DiagnosticStatus::Pass => state.theme.status.success,
        DiagnosticStatus::Warn => state.theme.status.warning,
        DiagnosticStatus::Fail => state.theme.status.error,
        DiagnosticStatus::Unknown | DiagnosticStatus::NotChecked => state.theme.status.info,
        DiagnosticStatus::NotApplicable => state.theme.text.muted,
    };
    Style::default().fg(color)
}

fn status_label(state: &AppState, status: DiagnosticStatus) -> &'static str {
    state.t().doctor_page.status_title(status)
}

fn ensure_selection(state: &mut AppState) {
    let findings = all_findings(state);
    let Some(first) = findings.first() else {
        state.doctor.selected = None;
        return;
    };
    if let Some(selected) = &state.doctor.selected
        && findings.iter().any(|finding| &finding.key() == selected)
    {
        return;
    }
    state.doctor.selected = Some(first.key());
}

fn selected_index(state: &AppState, findings: &[DiagnosticFinding]) -> Option<usize> {
    let selected = state.doctor.selected.as_deref()?;
    findings
        .iter()
        .position(|finding| finding.key() == selected)
}

fn move_selection(state: &mut AppState, delta: isize) {
    let findings = all_findings(state);
    if findings.is_empty() {
        state.doctor.selected = None;
        return;
    }
    let current = selected_index(state, &findings).unwrap_or(0);
    let next = if delta < 0 {
        current.saturating_sub(delta.unsigned_abs())
    } else {
        (current + delta as usize).min(findings.len() - 1)
    };
    state.doctor.selected = Some(findings[next].key());
}

// ---------------------------------------------------------------------------
// Keys
// ---------------------------------------------------------------------------

pub fn handle_key(state: &mut AppState, key: KeyEvent) -> Vec<Effect> {
    match key.code {
        KeyCode::Esc => {
            // The fact panel is not a navigation level: Esc is always the
            // page's one back. Enter collapses or expands the panel.
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
        KeyCode::PageDown => {
            move_selection(state, 10);
            Vec::new()
        }
        KeyCode::PageUp => {
            move_selection(state, -10);
            Vec::new()
        }
        KeyCode::Enter => {
            state.doctor.detail_open = !state.doctor.detail_open;
            Vec::new()
        }
        KeyCode::Char('r') | KeyCode::Char('R') => reload(state),
        _ => Vec::new(),
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
                if state.doctor.selected.as_deref() == Some(key.as_str()) {
                    state.doctor.detail_open = !state.doctor.detail_open;
                } else {
                    state.doctor.selected = Some(key.clone());
                    state.doctor.detail_open = true;
                }
            }
            Vec::new()
        }
        _ => Vec::new(),
    }
}

// ---------------------------------------------------------------------------
// Rendering
// ---------------------------------------------------------------------------

fn pad_display(text: &str, width: usize) -> String {
    let used = UnicodeWidthStr::width(text);
    let mut out = text.to_string();
    for _ in used..width {
        out.push(' ');
    }
    out
}

pub fn render_screen(frame: &mut Frame, area: Rect, state: &mut AppState) {
    let t = &state.t().doctor_page;
    let checked = state
        .doctor
        .report
        .as_ref()
        .map(|report| report.generated_at.as_str())
        .or(state.doctor.query_started.map(|_| ""))
        .map(|stamp| {
            if stamp.is_empty() {
                t.never_checked.to_string()
            } else {
                t.last_checked.replacen(
                    "{}",
                    &crate::render::text::sanitize_ui_text(&fmt_stamp(stamp)),
                    1,
                )
            }
        })
        .unwrap_or_else(|| t.never_checked.to_string());
    let mut status = vec![Span::styled(
        checked,
        Style::default().fg(state.theme.text.muted),
    )];
    if state.doctor.checking() {
        status.push(Span::styled(
            format!("   · {}", t.checking),
            Style::default().fg(state.theme.status.info),
        ));
    }
    if state.doctor.timed_out() {
        status.push(Span::styled(
            format!("   · {}", t.query_failed),
            Style::default().fg(state.theme.status.warning),
        ));
    }
    if !state.runtime_connected {
        status.push(Span::styled(
            format!("   · {}", t.disconnected),
            Style::default().fg(state.theme.status.warning),
        ));
    }
    let hint = t.footer;
    let page = crate::secondary::SecondaryPage {
        title: t.title,
        status,
        hint,
    };
    let layout = crate::secondary::layout(area, 0);
    crate::secondary::draw_header(frame, state, &layout, &page);
    crate::secondary::draw_footer(frame, &layout, &page, &state.theme);
    let body = layout.content;
    state.screen_rows.clear();
    if body.width == 0 || body.height == 0 {
        return;
    }

    let findings = all_findings(state);
    let t = &state.t().doctor_page;
    let mut lines: Vec<(Line<'static>, Option<String>)> = Vec::new();

    // Overview: every category, even an empty one, so "never checked" is
    // visible rather than absent.
    lines.push((
        Line::from(Span::styled(
            t.overview.to_string(),
            Style::default()
                .fg(state.theme.text.muted)
                .add_modifier(Modifier::BOLD),
        )),
        None,
    ));
    for category in CATEGORIES {
        let status = category_status_of(&findings, category);
        let (word, style) = match status {
            Some(status) => (status_label(state, status), status_style(state, status)),
            None => (
                t.status_not_checked,
                Style::default().fg(state.theme.text.muted),
            ),
        };
        lines.push((
            Line::from(vec![
                Span::styled(
                    format!("  {}", pad_display(t.category_title(category), 18)),
                    Style::default().fg(state.theme.text.primary),
                ),
                Span::styled(word.to_string(), style),
            ]),
            None,
        ));
    }
    lines.push((Line::default(), None));

    let problems = findings
        .iter()
        .filter(|finding| finding.status.is_problem())
        .count();
    let unchecked = findings
        .iter()
        .filter(|finding| {
            matches!(
                finding.status,
                DiagnosticStatus::Unknown | DiagnosticStatus::NotChecked
            )
        })
        .count();
    let heading = if problems == 0 {
        if unchecked == 0 {
            format!("{} · {}", t.evidence, t.no_issues)
        } else {
            format!(
                "{} · {}",
                t.evidence,
                t.no_problems_unchecked
                    .replacen("{}", &unchecked.to_string(), 1)
            )
        }
    } else {
        format!(
            "{} · {}",
            t.evidence,
            t.issue_count.replacen("{}", &problems.to_string(), 1)
        )
    };
    lines.push((
        Line::from(Span::styled(
            heading,
            Style::default()
                .fg(state.theme.text.muted)
                .add_modifier(Modifier::BOLD),
        )),
        None,
    ));
    if findings.is_empty() {
        lines.push((
            Line::from(Span::styled(
                t.empty.to_string(),
                Style::default().fg(state.theme.text.muted),
            )),
            None,
        ));
    }
    let status_width = findings
        .iter()
        .map(|finding| UnicodeWidthStr::width(status_label(state, finding.status)))
        .max()
        .unwrap_or(4)
        .clamp(4, 13);
    for finding in &findings {
        let key = finding.key();
        let selected = state.doctor.selected.as_deref() == Some(key.as_str());
        let marker = if selected { "›" } else { " " };
        let title = state.t().doctor_page.finding_title(&finding.id);
        let subject = finding
            .subject
            .as_deref()
            .map(|subject| format!(" · {subject}"))
            .unwrap_or_default();
        lines.push((
            Line::from(vec![
                Span::styled(
                    format!("{marker} "),
                    Style::default().fg(state.theme.accent.primary),
                ),
                Span::styled(
                    pad_display(status_label(state, finding.status), status_width),
                    status_style(state, finding.status),
                ),
                Span::raw("  "),
                Span::styled(
                    format!("{title}{subject}"),
                    Style::default().fg(state.theme.text.primary),
                ),
            ]),
            Some(key),
        ));
    }

    let detail_height = if state.doctor.detail_open {
        (body.height / 2).max(4).min(body.height.saturating_sub(3))
    } else {
        0
    };
    let list_height = body.height.saturating_sub(detail_height);
    let list_area = Rect {
        height: list_height,
        ..body
    };
    let selected_line = state.doctor.selected.as_deref().and_then(|selected| {
        lines
            .iter()
            .position(|(_, key)| key.as_deref() == Some(selected))
    });
    let max_offset = lines.len().saturating_sub(list_area.height as usize);
    let offset = selected_line
        .map(|line| line.saturating_sub(list_area.height.saturating_sub(1) as usize))
        .unwrap_or(0)
        .min(max_offset);
    let visible: Vec<Line<'static>> = lines
        .drain(..)
        .enumerate()
        .filter(|(index, _)| *index >= offset && *index < offset + list_area.height as usize)
        .map(|(index, (line, key))| {
            if let Some(key) = key {
                state.screen_rows.push((
                    Rect {
                        y: list_area.y + (index - offset) as u16,
                        height: 1,
                        ..list_area
                    },
                    key,
                ));
            }
            line
        })
        .collect();
    frame.render_widget(Paragraph::new(visible), list_area);

    if detail_height == 0 {
        return;
    }
    let rule_area = Rect {
        y: body.y + list_height,
        height: 1,
        ..body
    };
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(
            "─".repeat(rule_area.width as usize),
            Style::default().fg(state.theme.border.normal),
        ))),
        rule_area,
    );
    let detail_area = Rect {
        y: rule_area.y + rule_area.height,
        height: body.height.saturating_sub(list_height + rule_area.height),
        ..body
    };
    let Some(finding) = findings
        .iter()
        .find(|finding| Some(finding.key().as_str()) == state.doctor.selected.as_deref())
        .cloned()
    else {
        return;
    };
    let mut detail = vec![Line::from(vec![
        Span::styled(
            state.t().doctor_page.finding_title(&finding.id).to_string(),
            Style::default()
                .fg(state.theme.text.primary)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            format!(
                "  {} · {}",
                state.t().doctor_page.status_title(finding.status),
                state.t().doctor_page.category_title(finding.category)
            ),
            status_style(state, finding.status),
        ),
    ])];
    if let Some(subject) = &finding.subject {
        detail.push(Line::from(Span::styled(
            crate::render::text::sanitize_ui_text(&format!("{subject}  ·  {}", finding.key())),
            Style::default().fg(state.theme.text.muted),
        )));
    } else {
        detail.push(Line::from(Span::styled(
            crate::render::text::sanitize_ui_text(&finding.key()),
            Style::default().fg(state.theme.text.muted),
        )));
    }
    detail.push(Line::default());
    detail.push(Line::from(Span::styled(
        state.t().doctor_page.evidence.to_string(),
        Style::default()
            .fg(state.theme.text.muted)
            .add_modifier(Modifier::BOLD),
    )));
    for evidence in &finding.evidence {
        for line in
            crate::render::text::wrap(&format!("  · {evidence}"), detail_area.width as usize)
        {
            detail.push(Line::from(Span::styled(
                line,
                Style::default().fg(state.theme.text.muted),
            )));
        }
    }
    if let Some(action) = finding.action.as_deref()
        && let Some(sentence) = state.t().doctor_page.action_title(action)
    {
        detail.push(Line::default());
        detail.push(Line::from(Span::styled(
            format!("{}: {sentence}", state.t().doctor_page.action),
            Style::default().fg(state.theme.accent.primary),
        )));
    }
    frame.render_widget(Paragraph::new(detail), detail_area);
}

fn fmt_stamp(raw: &str) -> String {
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
        state
    }

    #[test]
    fn unknown_and_not_checked_are_never_painted_as_pass() {
        assert!(status_rank(DiagnosticStatus::Unknown) > status_rank(DiagnosticStatus::Pass));
        assert!(status_rank(DiagnosticStatus::NotChecked) > status_rank(DiagnosticStatus::Pass));
        assert!(status_rank(DiagnosticStatus::NotApplicable) < status_rank(DiagnosticStatus::Pass));
    }

    #[test]
    fn a_disconnected_runtime_is_a_confirmed_failure_not_unknown() {
        let mut state = state();
        state.runtime_connected = false;
        let finding = local_findings(&state)
            .into_iter()
            .find(|finding| finding.id == finding_id::RUNTIME_CONNECTION)
            .unwrap();
        assert_eq!(finding.status, DiagnosticStatus::Fail);
    }

    #[test]
    fn a_pending_crash_window_is_a_warning_with_an_action() {
        let mut state = state();
        state.snapshot_version = Some(1);
        state.interrupted_tool_calls = vec![leveler_client_protocol::UiInterruptedToolCall {
            call_id: "c1".into(),
            tool: "run_command".into(),
            pending_approval: false,
            state: leveler_client_protocol::UiInterruptedCallState::Unknown,
        }];
        let finding = local_findings(&state)
            .into_iter()
            .find(|finding| finding.id == finding_id::RUNTIME_RECOVERY)
            .unwrap();
        assert_eq!(finding.status, DiagnosticStatus::Warn);
        assert_eq!(finding.category, DiagnosticCategory::Recovery);
        assert_eq!(
            finding.action.as_deref(),
            Some(action_id::ACKNOWLEDGE_RECOVERY)
        );
    }

    #[test]
    fn a_stale_answer_does_not_replace_a_newer_query() {
        let mut state = state();
        let query = CommandId::generate();
        state.doctor.query = Some(query.clone());
        state.doctor.query_started = Some(Instant::now());
        let report = DiagnosticReport {
            generated_at: "t".into(),
            findings: vec![],
        };
        let requester = state.session_id.clone();
        apply_loaded(
            &mut state,
            &requester,
            &CommandId::generate(),
            report.clone(),
        );
        assert!(state.doctor.report.is_none(), "stale answer dropped");
        apply_loaded(&mut state, &requester, &query, report);
        assert!(state.doctor.report.is_some());
        assert!(!state.doctor.checking());
    }

    #[test]
    fn the_overview_never_reports_pass_when_a_check_was_not_run() {
        let mut state = state();
        state.doctor.report = Some(DiagnosticReport {
            generated_at: "t".into(),
            findings: vec![
                local_finding(
                    finding_id::SANDBOX_CAPABILITY,
                    None,
                    DiagnosticCategory::Sandbox,
                    DiagnosticStatus::Pass,
                    vec!["configured".into()],
                    None,
                ),
                local_finding(
                    finding_id::MCP_CONNECTION,
                    None,
                    DiagnosticCategory::Mcp,
                    DiagnosticStatus::NotChecked,
                    vec!["not exposed".into()],
                    None,
                ),
            ],
        });
        assert_eq!(
            category_status(&state, DiagnosticCategory::Mcp),
            Some(DiagnosticStatus::NotChecked)
        );
        assert_eq!(
            category_status(&state, DiagnosticCategory::Sandbox),
            Some(DiagnosticStatus::Pass)
        );
    }

    #[test]
    fn client_facts_win_over_a_runtime_answer_for_the_same_key() {
        let mut state = state();
        state.runtime_connected = false;
        state.doctor.report = Some(DiagnosticReport {
            generated_at: "t".into(),
            findings: vec![local_finding(
                finding_id::RUNTIME_CONNECTION,
                None,
                DiagnosticCategory::Runtime,
                DiagnosticStatus::Pass,
                vec!["stale claim".into()],
                None,
            )],
        });
        let merged = all_findings(&state);
        let connection = merged
            .iter()
            .find(|finding| finding.id == finding_id::RUNTIME_CONNECTION)
            .unwrap();
        assert_eq!(connection.status, DiagnosticStatus::Fail);
    }
}
