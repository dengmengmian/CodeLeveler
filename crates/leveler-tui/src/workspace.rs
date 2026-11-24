//! Modal workspace: one popup host over the live conversation.
//!
//! Pages keep their own state, keys and runtime calls. This module only owns
//! the shared frame, the tab grouping, and the keys that move between sibling
//! pages. Opening a tab never starts a review, a cleanup, or a turn.
//!
//! `Shift+Tab` stays the permission cycle (handled before any page key). Pages
//! that already use `Tab` keep it. Everywhere else `Tab` moves to the next
//! sibling, and `Ctrl+Left` / `Ctrl+Right` do the same on every page, including
//! the ones that own `Tab`.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph};
use unicode_width::UnicodeWidthStr;

use crate::action::Effect;
use crate::i18n::UiText;
use crate::screen::Screen;
use crate::state::AppState;

/// Hit targets published by the last paint. Empty when no modal is up.
#[derive(Debug, Clone, Default)]
pub struct Hits {
    pub popup: Option<Rect>,
    pub close: Option<Rect>,
    pub tabs: Vec<(Rect, Screen)>,
}

/// How a page's content wants to be laid out. This is a content classification;
/// it does not decide who hosts the page.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PageClass {
    /// Long read-only document: Plan, Goal, Review, Context, Doctor.
    Document,
    /// One-column object list: Sessions, Skills, Trust, Agents.
    List,
    /// A list with a detail/inspector beside or after it.
    ListDetail,
    /// Two peer panes, e.g. Diff and Background Tasks.
    Split,
    /// A file-backed output viewer.
    Viewer,
}

#[derive(Debug, Clone, Copy)]
struct SizeSpec {
    pct: u16,
    min: u16,
    max: u16,
    v_margin: u16,
    /// Compact lists stay a picker-sized card. Content pages have no cap:
    /// a taller terminal shows more of the document instead of wasting rows.
    max_height: u16,
}

impl PageClass {
    fn size(self) -> SizeSpec {
        match self {
            PageClass::Document => SizeSpec {
                pct: 90,
                min: 60,
                max: 140,
                v_margin: 3,
                max_height: u16::MAX,
            },
            PageClass::List => SizeSpec {
                pct: 70,
                min: 48,
                max: 110,
                v_margin: 4,
                max_height: 32,
            },
            PageClass::ListDetail | PageClass::Split => SizeSpec {
                pct: 90,
                min: 56,
                max: 120,
                v_margin: 1,
                max_height: u16::MAX,
            },
            PageClass::Viewer => SizeSpec {
                pct: 90,
                min: 60,
                max: 140,
                v_margin: 2,
                max_height: u16::MAX,
            },
        }
    }
}

/// Geometry of one frame. Pure: the same terminal size, screen and footer
/// height always yield the same rects, so a resize cannot leave a stale layout.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FrameGeom {
    pub popup: Rect,
    /// The popup's top border row. The title and `[×]` are painted on it.
    pub title_row: Rect,
    /// The sibling-tab row, inside the border. Empty for a single page.
    pub tab_row: Rect,
    /// The one host divider under the title or tabs.
    pub rule_row: Rect,
    /// The page body. Pages inset this once with their own gutter.
    pub content: Rect,
    /// The host shortcut row, inside the border and below the body.
    pub footer: Rect,
}

/// Every non-conversation screen except Memory, which already paints its own
/// overlay on top of the workbench.
pub fn hosts(screen: Screen) -> bool {
    !matches!(screen, Screen::Conversation | Screen::Memory)
}

/// Sibling tabs for `screen`. One entry means a named page with no switch.
pub fn tabs(screen: Screen) -> &'static [Screen] {
    match screen {
        Screen::Agents | Screen::Skills | Screen::Mcp | Screen::Trust => {
            &[Screen::Agents, Screen::Skills, Screen::Mcp, Screen::Trust]
        }
        Screen::Tasks | Screen::Goal | Screen::Plan | Screen::Schedules | Screen::Trace => &[
            Screen::Tasks,
            Screen::Goal,
            Screen::Plan,
            Screen::Schedules,
            Screen::Trace,
        ],
        Screen::Diff | Screen::Review | Screen::Output | Screen::Context => &[
            Screen::Diff,
            Screen::Review,
            Screen::Output,
            Screen::Context,
        ],
        Screen::Settings | Screen::Doctor | Screen::Clean => {
            &[Screen::Settings, Screen::Doctor, Screen::Clean]
        }
        _ => &[],
    }
}

pub fn tab_label(screen: Screen, t: &UiText) -> &'static str {
    let w = &t.workspace;
    match screen {
        Screen::Agents => w.agents,
        Screen::Skills => w.skills,
        Screen::Mcp => w.mcp,
        Screen::Trust => w.trust,
        Screen::Tasks => w.tasks,
        Screen::Goal => w.goal,
        Screen::Plan => w.plan,
        Screen::Schedules => w.schedules,
        Screen::Trace => w.trace,
        Screen::Diff => w.diff,
        Screen::Review => w.review,
        Screen::Output => w.output,
        Screen::Context => w.context,
        Screen::Settings => w.settings,
        Screen::Doctor => w.doctor,
        Screen::Clean => w.clean,
        Screen::Help => w.help,
        Screen::Find => w.find,
        Screen::Sessions => w.sessions,
        Screen::Tools => w.tools,
        Screen::Activity | Screen::ActivityList => w.activity,
        Screen::Team => w.team,
        Screen::Issue => w.issue,
        Screen::PendingSteer => w.steer,
        Screen::ForeignSessions => w.foreign,
        Screen::Conversation | Screen::Memory => "",
    }
}

/// Content layout class. This does not decide the host: every one of these
/// screens is still hosted by the same Modal Workspace in P1.
pub fn page_class(screen: Screen) -> PageClass {
    match screen {
        Screen::Diff | Screen::ActivityList => PageClass::Split,
        Screen::Tools
        | Screen::Tasks
        | Screen::Mcp
        | Screen::Settings
        | Screen::Trace
        | Screen::Help
        | Screen::Team
        | Screen::Schedules => PageClass::ListDetail,
        Screen::Sessions
        | Screen::ForeignSessions
        | Screen::Skills
        | Screen::Trust
        | Screen::Agents
        | Screen::Issue
        | Screen::Find
        | Screen::Clean => PageClass::List,
        Screen::Output => PageClass::Viewer,
        Screen::Plan
        | Screen::Goal
        | Screen::Review
        | Screen::Context
        | Screen::Doctor
        | Screen::PendingSteer
        | Screen::Activity => PageClass::Document,
        Screen::Conversation | Screen::Memory => PageClass::Document,
    }
}

fn group_title(screen: Screen, t: &UiText) -> Option<&'static str> {
    if tabs(screen).len() < 2 {
        return None;
    }
    let w = &t.workspace;
    match screen {
        Screen::Agents | Screen::Skills | Screen::Mcp | Screen::Trust => Some(w.group_extensions),
        Screen::Tasks | Screen::Goal | Screen::Plan | Screen::Schedules | Screen::Trace => {
            Some(w.group_runtime)
        }
        Screen::Diff | Screen::Review | Screen::Output | Screen::Context => Some(w.group_inspect),
        Screen::Settings | Screen::Doctor | Screen::Clean => Some(w.group_system),
        _ => None,
    }
}

/// The name painted on the card's title row. A named group owns the title;
/// the active page is identified by its tab. A single page uses its own name.
fn card_title(state: &AppState) -> String {
    let t = state.t();
    // A form is a page-level mode: its action name replaces the group title so
    // the user can see what Enter will create before reading any field.
    if state.active_screen == Screen::Schedules
        && let Some(form) = state.schedule.form.as_ref()
    {
        return if form.editing.is_some() {
            t.schedules.form_edit.to_string()
        } else {
            t.schedules.form_create.to_string()
        };
    }
    if let Some(group) = group_title(state.active_screen, t) {
        return group.to_string();
    }
    match state.active_screen {
        Screen::Help => state
            .help
            .detail
            .map(str::to_string)
            .unwrap_or_else(|| tab_label(Screen::Help, t).to_string()),
        Screen::Activity => match state.activity_open.as_ref() {
            Some(crate::activity::ActivityId::Child(_)) => t.activity_title_child.to_string(),
            _ => t.activity_title_background.to_string(),
        },
        Screen::ActivityList => t.background_list_title.to_string(),
        Screen::ForeignSessions => match state.foreign_sessions.tool {
            Some(leveler_client_protocol::UiForeignTool::ClaudeCode) => {
                t.screen_resume_claude.to_string()
            }
            Some(leveler_client_protocol::UiForeignTool::Codex) => {
                t.screen_resume_codex.to_string()
            }
            None => tab_label(Screen::ForeignSessions, t).to_string(),
        },
        other => tab_label(other, t).to_string(),
    }
}

fn count_text(fmt: &str, n: usize) -> String {
    fmt.replacen("{}", &n.to_string(), 1)
}

fn count_items(w: &crate::i18n::WorkspaceText, n: usize) -> String {
    count_text(
        if n == 1 {
            w.count_items_one
        } else {
            w.count_items
        },
        n,
    )
}

fn count_files(w: &crate::i18n::WorkspaceText, n: usize) -> String {
    count_text(
        if n == 1 {
            w.count_files_one
        } else {
            w.count_files
        },
        n,
    )
}

fn activity_status_label(status: crate::activity::ActivityStatus, t: &UiText) -> &'static str {
    use crate::activity::ActivityStatus;
    match status {
        ActivityStatus::Running => t.background_status_running,
        ActivityStatus::Waiting => t.tasks.status_waiting,
        ActivityStatus::Completed => t.background_status_completed,
        ActivityStatus::Failed => t.background_status_failed,
        ActivityStatus::Stopped => t.background_status_stopped,
        ActivityStatus::Lost => t.background_status_lost,
        ActivityStatus::Conflict => t.background_status_conflict,
        ActivityStatus::Interrupted | ActivityStatus::Unreported => t.background_status_unknown,
    }
}

/// A short, truthful page status for the title row. Only structured state is
/// read; a missing fact means no status, never a fabricated one.
fn page_status(state: &AppState) -> Option<String> {
    let t = state.t();
    let w = &t.workspace;
    match state.active_screen {
        Screen::Tasks => {
            let rows = crate::tasks::visible_rows(state);
            let loading = state.tasks.index_pending || state.tasks.schedule_pending;
            if rows.is_empty() {
                loading.then(|| t.tasks.loading.to_string())
            } else {
                let running = rows
                    .iter()
                    .filter(|row| matches!(row.status, crate::tasks::TaskStatus::Running))
                    .count();
                let waiting = rows
                    .iter()
                    .filter(|row| {
                        matches!(
                            row.status,
                            crate::tasks::TaskStatus::Waiting | crate::tasks::TaskStatus::Blocked
                        )
                    })
                    .count();
                let mut parts = vec![count_items(w, rows.len())];
                if running > 0 {
                    parts.push(count_text(w.count_running, running));
                }
                if waiting > 0 {
                    parts.push(count_text(w.count_waiting, waiting));
                }
                if loading {
                    parts.push(t.tasks.loading.to_string());
                }
                Some(parts.join(" · "))
            }
        }
        Screen::Doctor => {
            if state.doctor.checking() {
                Some(t.doctor_page.checking.to_string())
            } else if state.doctor.timed_out() {
                Some(t.doctor_page.query_failed.to_string())
            } else if let Some(report) = &state.doctor.report {
                let problems = report
                    .findings
                    .iter()
                    .filter(|finding| finding.status.is_problem())
                    .count();
                Some(if problems == 0 {
                    t.doctor_page.no_issues.to_string()
                } else {
                    count_text(t.doctor_page.issue_count, problems)
                })
            } else {
                Some(t.doctor_page.never_checked.to_string())
            }
        }
        Screen::Team => {
            let total = state.team.children.len();
            if total == 0 {
                None
            } else {
                let active = state.team.active().count();
                let mut parts = vec![count_items(w, total)];
                if active > 0 {
                    parts.push(format!("{active} {}", t.tasks.count_active));
                }
                Some(parts.join(" · "))
            }
        }
        Screen::ActivityList => {
            let summaries = crate::activity::summaries(state);
            if summaries.is_empty() {
                None
            } else {
                let running = summaries
                    .iter()
                    .filter(|s| matches!(s.status, crate::activity::ActivityStatus::Running))
                    .count();
                let mut parts = vec![count_items(w, summaries.len())];
                if running > 0 {
                    parts.push(count_text(w.count_running, running));
                }
                Some(parts.join(" · "))
            }
        }
        Screen::Activity => {
            let id = state.activity_open.as_ref()?;
            let summary = crate::activity::summaries(state)
                .into_iter()
                .find(|summary| &summary.id == id)?;
            Some(activity_status_label(summary.status, t).to_string())
        }
        Screen::Plan => {
            let plan = state.plan.as_ref().or_else(|| {
                state
                    .transcript
                    .items()
                    .iter()
                    .rev()
                    .find_map(|item| match item {
                        crate::transcript::TranscriptItem::Plan(plan) => Some(plan),
                        _ => None,
                    })
            })?;
            let (done, total) = crate::workbench::plan_done_total(plan);
            Some(
                t.plan_n_done
                    .replace("{done}", &done.to_string())
                    .replace("{total}", &total.to_string()),
            )
        }
        Screen::Goal => {
            let goal = state.active_goal.as_ref()?;
            let phase = match goal.phase() {
                crate::active_goal::GoalPhase::Running => t.goal_phase_running,
                crate::active_goal::GoalPhase::Paused => t.goal_phase_paused,
                crate::active_goal::GoalPhase::Resuming => t.goal_phase_resuming,
                crate::active_goal::GoalPhase::Completed => t.goal_phase_completed,
                crate::active_goal::GoalPhase::Failed => t.goal_phase_failed,
            };
            let elapsed = crate::status_line::fmt_elapsed(
                goal.elapsed_at(std::time::Instant::now()).as_secs(),
            );
            Some(format!("{phase} · {elapsed}"))
        }
        Screen::Review => {
            let report = state.review_report.as_ref()?;
            let status =
                crate::render::transcript_lines::review_status_label(report.summary.status, t);
            let findings = report.summary.counts.total();
            if findings == 0 {
                Some(status.to_string())
            } else {
                Some(format!("{status} · {}", count_items(w, findings as usize)))
            }
        }
        Screen::Sessions => {
            if state.sessions_loading {
                Some(t.sessions_loading.to_string())
            } else if state.sessions.is_empty() {
                None
            } else {
                Some(count_items(w, state.sessions.len()))
            }
        }
        Screen::ForeignSessions => {
            let count = state.foreign_sessions.sessions.len();
            (count > 0).then(|| count_items(w, count))
        }
        Screen::Diff => {
            let diff = state.diff.as_ref()?;
            Some(format!(
                "{} · +{}/-{}",
                count_files(w, diff.files.len()),
                diff.total_added(),
                diff.total_removed()
            ))
        }
        Screen::Mcp => {
            let count = state.mcp.servers.len();
            (count > 0).then(|| count_items(w, count))
        }
        Screen::Trust => {
            let count = state.trust.rows.len();
            (count > 0).then(|| count_items(w, count))
        }
        Screen::Skills => {
            let count = state.skills_page.rows.len();
            (count > 0).then(|| count_items(w, count))
        }
        Screen::Agents => {
            if !state.agents_page.loaded {
                Some(t.workspace.agents_loading.to_string())
            } else {
                let count = state.agents_page.entries.len();
                (count > 0).then(|| count_items(w, count))
            }
        }
        Screen::Issue => {
            let count = crate::issue::issue_summary(state).total;
            (count > 0).then(|| count_items(w, count as usize))
        }
        Screen::Schedules => {
            let count = state.schedule.items.len();
            (count > 0).then(|| count_items(w, count))
        }
        Screen::Context => {
            let loaded = state.context.loaded.as_ref()?;
            let capacity = loaded
                .input_capacity_tokens
                .or(loaded.context_window_tokens)?;
            Some(format!("{}/{}", loaded.used_tokens, capacity))
        }
        _ => None,
    }
}

fn default_footer_rows(screen: Screen, inner_height: u16) -> u16 {
    if inner_height < 7 {
        0
    } else if tabs(screen).len() > 1 && inner_height >= 12 {
        2
    } else {
        1
    }
}

/// Popup rect for `area`. The size depends on the page's content class, not on
/// the page's identity: a document gets room, a list stays compact.
fn popup_rect(area: Rect, class: PageClass) -> Rect {
    let spec = class.size();
    let tight = area.width < 64 || area.height < 18;
    if tight {
        return area;
    }
    let min_w = spec.min.min(area.width);
    let max_w = spec.max.min(area.width);
    let mut width = (u32::from(area.width) * u32::from(spec.pct) / 100) as u16;
    width = width.clamp(min_w, max_w.max(min_w));
    width = width.min(area.width);
    let mut height = area.height.saturating_sub(spec.v_margin.saturating_mul(2));
    height = height.min(spec.max_height);
    if height < 12 {
        height = area.height;
    }
    height = height.min(area.height);
    let x = area.x + area.width.saturating_sub(width) / 2;
    let y = area.y + area.height.saturating_sub(height) / 2;
    Rect {
        x,
        y,
        width,
        height,
    }
}

/// Geometry with the default footer reservation. Interactive rendering uses
/// [`frame_geom_with_footer`] so the reserved rows and the painted rows agree.
pub fn frame_geom(area: Rect, screen: Screen) -> FrameGeom {
    let class = page_class(screen);
    let popup = popup_rect(area, class);
    let inner_height = popup.height.saturating_sub(2);
    frame_geom_with_footer(area, screen, default_footer_rows(screen, inner_height))
}

/// Geometry with an explicit footer row count (0–2), predicted by the same
/// HintBar layout that paints it.
pub fn frame_geom_with_footer(area: Rect, screen: Screen, footer_rows: u16) -> FrameGeom {
    if area.width == 0 || area.height == 0 {
        return FrameGeom {
            popup: area,
            title_row: Rect::default(),
            tab_row: Rect::default(),
            rule_row: Rect::default(),
            content: area,
            footer: Rect::default(),
        };
    }
    let class = page_class(screen);
    let popup = popup_rect(area, class);
    // A border needs two columns and two rows. Below that the page gets the
    // whole popup and draws its own shell.
    if popup.width < 8 || popup.height < 12 {
        return FrameGeom {
            popup,
            title_row: Rect::default(),
            tab_row: Rect::default(),
            rule_row: Rect::default(),
            content: popup,
            footer: Rect::default(),
        };
    }
    let inner = Rect {
        x: popup.x + 1,
        y: popup.y + 1,
        width: popup.width.saturating_sub(2),
        height: popup.height.saturating_sub(2),
    };
    let siblings = tabs(screen);
    let tab_h = if siblings.len() > 1 && inner.height >= 8 {
        1
    } else {
        0
    };
    let rule_y = inner.y + tab_h;
    let rule_row = Rect {
        x: inner.x,
        y: rule_y,
        width: inner.width,
        height: 1,
    };
    let footer_h = footer_rows
        .min(2)
        .min(inner.height.saturating_sub(tab_h + 2));
    let footer = Rect {
        x: inner.x,
        y: inner.y + inner.height.saturating_sub(footer_h),
        width: inner.width,
        height: footer_h,
    };
    let content_y = rule_y.saturating_add(1);
    let content = Rect {
        x: inner.x,
        y: content_y,
        width: inner.width,
        height: footer.y.saturating_sub(content_y),
    };
    let title_row = Rect {
        x: popup.x,
        y: popup.y,
        width: popup.width,
        height: 1,
    };
    let tab_row = Rect {
        x: inner.x,
        y: inner.y,
        width: inner.width,
        height: tab_h,
    };
    FrameGeom {
        popup,
        title_row,
        tab_row,
        rule_row,
        content,
        footer,
    }
}

/// The page is at its root list: workspace keys may move between siblings.
/// A search field, a form, a confirmation or an open detail keeps the key.
pub fn nav_free(state: &AppState) -> bool {
    match state.active_screen {
        Screen::Skills => !state.skills_page.detail_open && !state.skills_page.searching,
        Screen::Trust => !state.trust.detail_open,
        Screen::Mcp => {
            state.mcp.form.is_none()
                && state.mcp.confirm_remove.is_none()
                && !state.mcp.detail_open
                && !state.mcp.filter_focus
        }
        Screen::Tasks => !state.tasks.detail_open,
        Screen::Trace => !state.trace.inspect,
        Screen::Schedules => state.schedule.form.is_none() && state.schedule.detail.is_none(),
        Screen::Settings => !state.settings.detail_open && !state.settings.filter_focus,
        Screen::Agents => !state.agents_page.show_detail && !state.agents_page.searching,
        Screen::Help => state.help.detail.is_none(),
        Screen::Clean => !state.clean.details,
        Screen::Find => !state.find.searching,
        _ => true,
    }
}

/// Pages whose `Tab` already means something else. `Ctrl+Left` / `Ctrl+Right`
/// still move between siblings on these pages.
pub fn page_uses_tab(screen: Screen) -> bool {
    matches!(
        screen,
        Screen::Tools
            | Screen::Tasks
            | Screen::Mcp
            | Screen::Trace
            | Screen::Settings
            | Screen::Team
            | Screen::Help
            | Screen::Find
            | Screen::Context
            | Screen::ActivityList
    )
}

/// Workspace chrome keys. `None` means the page still owns the key.
pub fn chrome_key(state: &mut AppState, key: &KeyEvent) -> Option<Vec<Effect>> {
    let siblings = tabs(state.active_screen);
    if siblings.len() < 2 || !nav_free(state) {
        return None;
    }
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    let plain_tab = key.code == KeyCode::Tab && key.modifiers.is_empty();
    let forward = match key.code {
        KeyCode::Right if ctrl => true,
        KeyCode::Left if ctrl => false,
        KeyCode::Tab if ctrl => true,
        KeyCode::Tab if plain_tab && !page_uses_tab(state.active_screen) => true,
        _ => return None,
    };
    Some(activate(
        state,
        step(siblings, state.active_screen, forward),
    ))
}

/// Show `screen` without re-running a page's toggle-to-close, and without
/// repeating a read the page already holds.
pub fn activate(state: &mut AppState, screen: Screen) -> Vec<Effect> {
    if state.active_screen == screen {
        return Vec::new();
    }
    state.active_screen = screen;
    state.screen_scroll = 0;
    state.page_focus = crate::layout::SplitFocus::Primary;
    state.page_scroll = 0;
    state.page_split = None;
    // A tab change is a sibling, not a drill-in. Esc at the root closes the
    // popup instead of walking back through the page that opened the previous
    // tab.
    state.detail_return = None;
    ensure(state, screen)
}

fn step(siblings: &[Screen], current: Screen, forward: bool) -> Screen {
    let index = siblings.iter().position(|s| *s == current).unwrap_or(0);
    let next = if forward {
        (index + 1) % siblings.len()
    } else {
        (index + siblings.len() - 1) % siblings.len()
    };
    siblings[next]
}

fn ensure(state: &mut AppState, screen: Screen) -> Vec<Effect> {
    match screen {
        Screen::Skills => {
            if state.skills_page.rows.is_empty() {
                crate::skills::refresh(state);
            }
            Vec::new()
        }
        Screen::Trust => {
            if state.trust.rows.is_empty() {
                crate::trust::refresh(state);
            }
            Vec::new()
        }
        Screen::Mcp => {
            if state.mcp.servers.is_empty() && state.mcp.query.is_none() {
                crate::mcp::reload(state)
            } else {
                Vec::new()
            }
        }
        Screen::Agents => {
            if !state.agents_page.loaded && state.agents_list_query.is_none() {
                crate::agents_view::request_list(state)
            } else {
                Vec::new()
            }
        }
        Screen::Tasks => {
            if state.tasks.index.is_none() && !state.tasks.index_pending {
                crate::tasks::refresh(state)
            } else {
                Vec::new()
            }
        }
        Screen::Schedules => {
            if state.schedule.last_refresh.is_none() && state.schedule.list_query.is_none() {
                state.schedule.last_refresh = Some(std::time::Instant::now());
                vec![
                    crate::schedules::list_effect(state),
                    crate::schedules::status_effect(state),
                ]
            } else {
                Vec::new()
            }
        }
        Screen::Doctor => {
            if state.doctor.report.is_none() && state.doctor.query.is_none() {
                crate::doctor::reload(state)
            } else {
                Vec::new()
            }
        }
        Screen::Settings => {
            if state.settings.snapshot.is_none() && state.settings.query.is_none() {
                crate::settings::reload(state)
            } else {
                Vec::new()
            }
        }
        Screen::Diff => {
            if state.diff.is_some() || state.diff_pending || state.diff_error.is_some() {
                return Vec::new();
            }
            if !state.runtime_connected {
                state.diff_error = Some(state.t().connection_lost.to_string());
                return Vec::new();
            }
            state.diff_pending = true;
            let query_id = leveler_client_protocol::CommandId::generate();
            state.diff_query = Some(query_id.clone());
            vec![Effect::Send(
                leveler_client_protocol::ClientCommand::RequestDiff {
                    session_id: state.session_id.clone(),
                    query_id: Some(query_id),
                },
            )]
        }
        Screen::Trace => {
            if state.trace.loaded.is_some() || state.trace.error.is_some() {
                return Vec::new();
            }
            if !state.runtime_connected {
                state.trace.error = Some(state.t().connection_lost.to_string());
                return Vec::new();
            }
            vec![Effect::Send(crate::observability::issue_query(
                &mut state.trace,
                state.session_id.clone(),
                None,
                0,
                80,
            ))]
        }
        Screen::Context => {
            if state.context.loaded.is_some()
                || state.usage.loaded.is_some()
                || state.stats_error.is_some()
            {
                return Vec::new();
            }
            if !state.runtime_connected {
                state.stats_error = Some(state.t().connection_lost.to_string());
                return Vec::new();
            }
            vec![
                Effect::Send(crate::context::issue_query(state)),
                Effect::Send(crate::usage::query(state)),
            ]
        }
        Screen::Sessions => {
            if state.sessions_known || state.sessions_loading || !state.all_sessions.is_empty() {
                return Vec::new();
            }
            if !state.runtime_connected {
                state.sessions_error = Some(state.t().connection_lost.to_string());
                return Vec::new();
            }
            state.sessions_known = true;
            state.sessions_loading = true;
            vec![Effect::Send(
                leveler_client_protocol::ClientCommand::RequestSessionListFor {
                    requester_session_id: state.session_id.clone(),
                },
            )]
        }
        Screen::Clean => {
            if state.clean.stage != crate::clean::CleanStage::Idle {
                return Vec::new();
            }
            state.clean.begin_scan();
            vec![Effect::StartCleanScan]
        }
        // Review shows the report the user already opened. It never starts one.
        _ => Vec::new(),
    }
}

/// Mouse on the chrome. `None` means the event is inside the page body.
/// A click outside the popup is one Esc. A click during a form or a
/// confirmation is swallowed so it cannot reach the conversation underneath.
pub fn chrome_mouse(state: &mut AppState, mouse: MouseEvent) -> Option<Vec<Effect>> {
    if !hosts(state.active_screen) {
        state.workspace_hover = None;
        return None;
    }
    let Some(popup) = state.workspace_hits.popup else {
        state.workspace_hover = None;
        return None;
    };
    let point = ratatui::layout::Position {
        x: mouse.column,
        y: mouse.row,
    };
    let inside = popup.contains(point);
    if matches!(mouse.kind, MouseEventKind::Moved) {
        let hover = if !inside {
            None
        } else if state
            .workspace_hits
            .close
            .is_some_and(|rect| rect.contains(point))
        {
            Some(WorkspaceHover::Close)
        } else {
            state
                .workspace_hits
                .tabs
                .iter()
                .find(|(rect, _)| rect.contains(point))
                .map(|(_, screen)| WorkspaceHover::Tab(*screen))
        };
        state.workspace_hover = hover;
        // Chrome hover is published, but the page still owns row hover: a
        // movement over the body must reach its own hit map.
        return None;
    }
    match mouse.kind {
        MouseEventKind::Down(MouseButton::Left) => {
            if let Some((_, screen)) = state
                .workspace_hits
                .tabs
                .iter()
                .find(|(rect, _)| rect.contains(point))
            {
                let screen = *screen;
                return Some(activate(state, screen));
            }
            if state
                .workspace_hits
                .close
                .is_some_and(|rect| rect.contains(point))
            {
                return Some(crate::reducer::screen_nav::back(state));
            }
            if !inside {
                return if nav_free(state) {
                    Some(crate::reducer::screen_nav::back(state))
                } else {
                    Some(Vec::new())
                };
            }
            None
        }
        MouseEventKind::ScrollUp | MouseEventKind::ScrollDown if !inside => Some(Vec::new()),
        _ => None,
    }
}

/// Which host chrome element the pointer is over. Painted from the same
/// geometry that publishes the click hits.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorkspaceHover {
    Close,
    Tab(Screen),
}

/// The action a structured hint stands for. Only host actions are dispatched
/// from the footer in P1; page keys remain page-owned and are shown as keys.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HintAction {
    Back,
    SwitchTab,
    PageKey,
    /// Text with no key (e.g. "type to search"). It explains, it cannot be
    /// pressed, and it is never counted as a key handler.
    Info,
}

/// One resolved footer hint. `key` is the exact key name, `label` is already
/// localized, and `priority` decides what is dropped when width is tight.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hint {
    pub key: String,
    pub label: String,
    pub priority: u8,
    pub action: HintAction,
}

impl Hint {
    fn text(&self) -> String {
        if self.label.is_empty() {
            self.key.clone()
        } else {
            format!("{} {}", self.key, self.label)
        }
    }
}

fn split_hint_clause(clause: &str) -> (String, String) {
    let clause = clause.trim();
    if let Some((key, label)) = clause.split_once(' ') {
        (key.to_string(), label.trim().to_string())
    } else {
        (clause.to_string(), String::new())
    }
}

fn is_back_clause(key: &str, label: &str) -> bool {
    let lower = label.to_ascii_lowercase();
    key.eq_ignore_ascii_case("esc")
        || label.contains("返回")
        || lower == "back"
        || lower.starts_with("back ")
}

fn is_switch_clause(key: &str) -> bool {
    key == "Tab"
        || key.eq_ignore_ascii_case("Shift+Tab")
        || key.contains("Ctrl+←")
        || key.contains("Ctrl+→")
}

/// Legacy page hints use one of three separators: a line break, `" · "`, or
/// runs of two spaces. Splitting only on the first would swallow every other
/// key into one clause and then drop the whole line as a back clause.
fn page_hint_clauses(line: &str) -> Vec<&str> {
    let separated: Vec<&str> = if line.contains(" · ") {
        line.split(" · ").collect()
    } else if line.contains("  ") {
        line.split("  ").collect()
    } else {
        vec![line]
    };
    separated
        .into_iter()
        .map(str::trim)
        .filter(|clause| !clause.is_empty())
        .collect()
}

/// The current page's footer, read from the same typed state the page renders.
/// Called before geometry, so a page switch can never reserve rows from the
/// previous page's hint or paint a stale key for one frame.
pub fn current_page_hint(state: &AppState) -> Option<String> {
    let t = state.t();
    let hint = match state.active_screen {
        Screen::Conversation | Screen::Memory => return None,
        Screen::Tools => crate::tool_cell::tools_footer_hint(state.size.0 as usize, t),
        Screen::Diff => t.diff_footer_hint.to_string(),
        Screen::Find => t.find.hint.to_string(),
        Screen::Sessions => t.sessions_footer_hint.to_string(),
        Screen::ForeignSessions => t.foreign_sessions_footer_hint.to_string(),
        Screen::Activity => {
            let Some(id) = state.activity_open.clone() else {
                return Some(t.activity_esc.to_string());
            };
            let Some(summary) = crate::activity::summaries(state)
                .into_iter()
                .find(|summary| summary.id == id)
            else {
                return Some(t.activity_esc.to_string());
            };
            crate::render::activity_footer_hint(state, &id, &summary, t)
        }
        Screen::ActivityList => {
            let list = crate::activity::background_job_list(state);
            crate::render::background_page_hint(state, &list, t)
        }
        Screen::Team => {
            if state.team.children.is_empty() {
                t.team_page_hint_empty.to_string()
            } else {
                t.team_page_hint.to_string()
            }
        }
        Screen::Issue => t.issue_page_hint.to_string(),
        Screen::Plan => t.plan_detail_hint.to_string(),
        Screen::PendingSteer => t.steer_hint.to_string(),
        Screen::Review => t.review.report_hint.to_string(),
        Screen::Goal => t.goal_detail_hint.to_string(),
        Screen::Help => {
            if state.help.detail.is_some() {
                t.help_page.footer_detail.to_string()
            } else {
                t.help_page.footer_list.to_string()
            }
        }
        Screen::Doctor => t.doctor_page.footer.to_string(),
        Screen::Mcp => {
            let page = &t.mcp_page;
            if state.mcp.form.is_some() {
                page.footer_form.to_string()
            } else if state.mcp.detail_open {
                page.footer_detail.to_string()
            } else {
                page.footer_list.to_string()
            }
        }
        Screen::Settings => t.settings_page.footer.to_string(),
        Screen::Tasks => {
            if state.tasks.detail_open {
                t.tasks.footer_detail.to_string()
            } else {
                t.tasks.footer.to_string()
            }
        }
        Screen::Trace => t.trace_hint.to_string(),
        Screen::Context => t.stats_footer_hint.to_string(),
        Screen::Clean => t.clean.footer_hint.to_string(),
        Screen::Trust => {
            if state.trust.detail_open {
                t.trust.hint_detail.to_string()
            } else {
                t.trust.hint_list.to_string()
            }
        }
        Screen::Skills => {
            let page = &t.skills;
            if state.skills_page.searching {
                page.hint_search.to_string()
            } else if state.skills_page.detail_open {
                page.hint_detail.to_string()
            } else {
                page.hint_list.to_string()
            }
        }
        Screen::Agents => {
            let w = &t.workspace;
            if state.agents_page.searching {
                w.agents_hint_search.to_string()
            } else if state.agents_page.show_detail {
                w.agents_hint_detail.to_string()
            } else {
                w.agents_hint_list.to_string()
            }
        }
        Screen::Schedules => {
            let page = &t.schedules;
            if state.schedule.form.is_some() {
                page.footer_form.to_string()
            } else if state.schedule.detail.is_some() {
                page.footer_detail.to_string()
            } else {
                page.footer_list.to_string()
            }
        }
        Screen::Output => t.output_screen_hint.to_string(),
    };
    Some(hint)
}

/// Every footer hint for the current page, in paint order. Back is always the
/// last hint; a sibling switch is present only when the chrome actually
/// handles it in this state.
pub fn hints(state: &AppState) -> Vec<Hint> {
    let t = state.t();
    let siblings = tabs(state.active_screen);
    let mut out = Vec::new();
    let current = current_page_hint(state);
    let page = current.as_deref().or(state.card_hint.as_deref());
    if let Some(page) = page {
        for line in page.lines() {
            for clause in page_hint_clauses(line) {
                let clause = clause.trim();
                if clause.is_empty() {
                    continue;
                }
                let (key, label) = split_hint_clause(clause);
                if key.is_empty() && label.is_empty() {
                    continue;
                }
                if is_back_clause(&key, &label) {
                    continue;
                }
                if is_switch_clause(&key) && (siblings.len() < 2 || !nav_free(state)) {
                    continue;
                }
                if out
                    .iter()
                    .any(|hint: &Hint| hint.key == key && hint.label == label)
                {
                    continue;
                }
                let info = label.is_empty();
                out.push(Hint {
                    key,
                    label,
                    priority: if info {
                        200
                    } else {
                        10 + out.len().min(100) as u8
                    },
                    action: if info {
                        HintAction::Info
                    } else {
                        HintAction::PageKey
                    },
                });
            }
        }
    }
    if siblings.len() > 1 && nav_free(state) {
        let key = if page_uses_tab(state.active_screen) {
            "Ctrl+←/→"
        } else {
            "Tab"
        };
        if !out.iter().any(|hint| is_switch_clause(&hint.key)) {
            out.push(Hint {
                key: key.to_string(),
                label: t.workspace.switch.to_string(),
                priority: 5,
                action: HintAction::SwitchTab,
            });
        }
    }
    // The back action is never optional: it is the one host operation that
    // must survive every width.
    out.push(Hint {
        key: "Esc".to_string(),
        label: t.workspace.back.to_string(),
        priority: 255,
        action: HintAction::Back,
    });
    out
}

fn hint_text_width(hint: &Hint) -> usize {
    UnicodeWidthStr::width(hint.text().as_str())
}

fn hint_rows_needed(hints: &[&Hint], width: usize) -> usize {
    if hints.is_empty() || width == 0 {
        return 0;
    }
    let sep = UnicodeWidthStr::width(" · ");
    let mut rows = 1usize;
    let mut used = 0usize;
    for hint in hints {
        let w = hint_text_width(hint);
        if used == 0 {
            used = w;
        } else if used + sep + w <= width {
            used += sep + w;
        } else {
            rows += 1;
            used = w;
        }
    }
    rows
}

fn join_hint_rows<'a>(hints: &[&'a Hint], width: usize) -> Vec<Vec<&'a Hint>> {
    if hints.is_empty() || width == 0 {
        return Vec::new();
    }
    let sep = UnicodeWidthStr::width(" · ");
    let mut rows: Vec<Vec<&Hint>> = vec![Vec::new()];
    let mut used = 0usize;
    for hint in hints {
        let w = hint_text_width(hint);
        let row = rows.last_mut().expect("one row exists");
        if row.is_empty() {
            row.push(*hint);
            used = w;
        } else if used + sep + w <= width {
            row.push(*hint);
            used += sep + w;
        } else {
            rows.push(vec![*hint]);
            used = w;
        }
    }
    rows
}

/// Pack hints into at most `max_rows` rows, dropping lower-priority page keys
/// before the back key. Rendering and geometry prediction call this same
/// function, so the reserved footer height and the painted rows agree.
pub fn hint_rows(hints: &[Hint], width: usize, max_rows: usize) -> Vec<Vec<&Hint>> {
    if width == 0 || max_rows == 0 || hints.is_empty() {
        return Vec::new();
    }
    let Some(back) = hints.iter().rev().find(|h| h.action == HintAction::Back) else {
        return join_hint_rows(&hints.iter().collect::<Vec<_>>(), width)
            .into_iter()
            .take(max_rows)
            .collect();
    };
    let body: Vec<&Hint> = hints
        .iter()
        .filter(|hint| hint.action != HintAction::Back)
        .collect();
    // Drop the least important page keys first, but keep the source order on
    // screen. The switch hint outranks ordinary page keys so a wide footer
    // always explains how to change tabs.
    let mut drop_order: Vec<usize> = (0..body.len()).collect();
    drop_order.sort_by_key(|index| std::cmp::Reverse(body[*index].priority));
    for removed in 0..=body.len() {
        let dropped: std::collections::HashSet<usize> =
            drop_order.iter().copied().take(removed).collect();
        let mut trial: Vec<&Hint> = body
            .iter()
            .enumerate()
            .filter(|(index, _)| !dropped.contains(index))
            .map(|(_, hint)| *hint)
            .collect();
        trial.push(back);
        if hint_rows_needed(&trial, width) <= max_rows {
            return join_hint_rows(&trial, width);
        }
    }
    // Even a single back hint is wider than the row: truncate through the
    // painter rather than dropping it.
    vec![vec![back]]
}

/// Geometry for the current state, with the footer height predicted from the
/// same `hint_rows` layout that `paint_footer` uses.
pub fn frame_geom_for(state: &AppState, area: Rect) -> FrameGeom {
    let provisional = frame_geom(area, state.active_screen);
    if provisional.footer.height == 0 {
        return provisional;
    }
    let width = inset_row(provisional.footer).width as usize;
    let predicted = hint_rows(&hints(state), width, 2).len() as u16;
    let floor = default_footer_rows(
        state.active_screen,
        provisional.popup.height.saturating_sub(2),
    );
    frame_geom_with_footer(area, state.active_screen, predicted.max(floor))
}

pub fn paint(frame: &mut Frame, state: &mut AppState, geom: &FrameGeom) {
    let theme = state.theme.clone();
    paint_scrim(frame, frame.area(), &theme);
    let screen = state.active_screen;
    let siblings = tabs(screen);
    frame.render_widget(Clear, geom.popup);
    // Borderless: the page paints the whole popup. A title or close glyph
    // drawn first would sit under the page and still own the click.
    let borderless = geom.content == geom.popup;
    if !borderless {
        let block = Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(theme.border.normal))
            .style(Style::default().bg(theme.surface.elevated));
        frame.render_widget(block, geom.popup);
        if geom.popup.width > 2 && geom.popup.height > 2 {
            frame.render_widget(
                Block::default().style(Style::default().bg(theme.surface.elevated)),
                Rect {
                    x: geom.popup.x + 1,
                    y: geom.popup.y + 1,
                    width: geom.popup.width - 2,
                    height: geom.popup.height - 2,
                },
            );
        }
    }
    let mut hits = Hits {
        popup: Some(geom.popup),
        close: None,
        tabs: Vec::new(),
    };
    if borderless {
        state.modal_chrome = false;
        state.modal_named = false;
        state.workspace_hits = hits;
        return;
    }
    state.modal_chrome = true;
    // The host always names the page (group title or single page title), so
    // no page may paint a second `← title` or a page-level rule.
    state.modal_named = true;
    let title = card_title(state);
    let status = page_status(state);
    if geom.title_row.height > 0 {
        hits.close = paint_title(
            frame,
            geom.title_row,
            &title,
            status.as_deref(),
            &theme,
            state.workspace_hover,
        );
    }
    if geom.tab_row.height > 0 && !siblings.is_empty() {
        hits.tabs = paint_tabs(
            frame,
            geom.tab_row,
            siblings,
            screen,
            state.t(),
            &theme,
            state.workspace_hover,
        );
    }
    if geom.rule_row.height > 0 {
        paint_rule(frame, geom.rule_row, &theme);
    }
    if let Some(rect) = hits.close {
        state.back_hit = Some((rect.y, rect.x, rect.x.saturating_add(rect.width)));
    }
    state.workspace_hits = hits;
}

/// Fill the whole terminal with an opaque scrim. This deliberately erases the
/// workbench glyphs; the workbench state is untouched and repaints when the
/// modal closes.
fn paint_scrim(frame: &mut Frame, area: Rect, theme: &crate::theme::Theme) {
    let buf = frame.buffer_mut();
    let style = Style::default()
        .fg(theme.surface.scrim)
        .bg(theme.surface.scrim);
    for y in area.y..area.y.saturating_add(area.height) {
        for x in area.x..area.x.saturating_add(area.width) {
            if let Some(cell) = buf.cell_mut((x, y)) {
                cell.reset();
                cell.set_symbol(" ");
                cell.set_style(style);
            }
        }
    }
}

fn paint_title(
    frame: &mut Frame,
    row: Rect,
    title: &str,
    status: Option<&str>,
    theme: &crate::theme::Theme,
    hover: Option<WorkspaceHover>,
) -> Option<Rect> {
    if row.width < 10 {
        return None;
    }
    let buf = frame.buffer_mut();
    let y = row.y;
    let close_w = UnicodeWidthStr::width(CLOSE) as u16;
    let close_x = row.x + row.width.saturating_sub(close_w + 2);
    let close_rect = Rect {
        x: close_x,
        y,
        width: close_w,
        height: 1,
    };
    let border_style = Style::default().fg(theme.border.normal);
    // The block already painted the top border; replace its first run with
    // `─ ` so the title reads as part of the border.
    buf.set_stringn(row.x + 1, y, "─ ", 2, border_style);
    let title_style = Style::default()
        .fg(theme.text.primary)
        .add_modifier(Modifier::BOLD);
    let status_room = close_x.saturating_sub(row.x + 3);
    let status_width = status
        .map(|status| UnicodeWidthStr::width(status) as u16)
        .unwrap_or(0)
        .min(status_room.saturating_sub(10));
    let status_x = close_x.saturating_sub(status_width + 2);
    let title_limit = if status_width > 0 {
        status_x.saturating_sub(2)
    } else {
        close_x.saturating_sub(2)
    };
    let title_room = title_limit.saturating_sub(row.x + 3) as usize;
    if title_room > 0 {
        let title_width = UnicodeWidthStr::width(title) as u16;
        let shown = crate::render::truncate_display(title, title_room);
        let shown_width = UnicodeWidthStr::width(shown.as_str()) as u16;
        buf.set_stringn(row.x + 3, y, &shown, title_room, title_style);
        let title_end = row.x + 3 + shown_width.min(title_width);
        if title_end < close_x.saturating_sub(1) {
            buf.set_stringn(title_end, y, " ", 1, border_style);
        }
    }
    if status_width > 0
        && let Some(status) = status
    {
        let shown = crate::render::truncate_display(status, status_width as usize);
        let shown_width = UnicodeWidthStr::width(shown.as_str()) as u16;
        if status_x > row.x + 3 {
            buf.set_stringn(status_x.saturating_sub(1), y, " ", 1, border_style);
        }
        buf.set_stringn(
            status_x,
            y,
            &shown,
            status_width as usize,
            Style::default().fg(theme.text.muted),
        );
        let status_end = status_x.saturating_add(shown_width);
        if status_end < close_x.saturating_sub(1) {
            buf.set_stringn(status_end, y, " ", 1, border_style);
        }
    }
    if close_x > row.x + 1 {
        buf.set_stringn(close_x.saturating_sub(1), y, " ", 1, border_style);
    }
    let close_style = if matches!(hover, Some(WorkspaceHover::Close)) {
        Style::default()
            .fg(theme.text.primary)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(theme.text.muted)
    };
    buf.set_stringn(close_rect.x, y, CLOSE, close_w as usize, close_style);
    let after_close = close_rect.x.saturating_add(close_w);
    if after_close < row.x + row.width.saturating_sub(1) {
        buf.set_stringn(after_close, y, " ", 1, border_style);
    }
    Some(close_rect)
}

fn paint_tabs(
    frame: &mut Frame,
    row: Rect,
    siblings: &[Screen],
    current: Screen,
    t: &UiText,
    theme: &crate::theme::Theme,
    hover: Option<WorkspaceHover>,
) -> Vec<(Rect, Screen)> {
    let mut hits = Vec::new();
    if row.width == 0 {
        return hits;
    }
    let pad = 2u16.min(row.width);
    let mut x = row.x + pad;
    let limit = row.x + row.width.saturating_sub(pad);
    for screen in siblings {
        if x >= limit {
            break;
        }
        let selected = *screen == current;
        let label = tab_label(*screen, t);
        let text = if selected {
            format!("[{label}]")
        } else {
            label.to_string()
        };
        let width = UnicodeWidthStr::width(text.as_str()) as u16;
        let room = limit.saturating_sub(x);
        if width == 0 || room < 3 {
            break;
        }
        let drawn = width.min(room);
        let rect = Rect {
            x,
            y: row.y,
            width: drawn,
            height: 1,
        };
        let style = if selected {
            Style::default()
                .fg(theme.accent.primary)
                .add_modifier(Modifier::BOLD)
        } else if matches!(hover, Some(WorkspaceHover::Tab(tab)) if tab == *screen) {
            Style::default()
                .fg(theme.text.primary)
                .bg(theme.surface.selection)
        } else {
            Style::default().fg(theme.text.muted)
        };
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                crate::render::truncate_display(&text, drawn as usize),
                style,
            ))),
            rect,
        );
        hits.push((rect, *screen));
        x = x.saturating_add(drawn).saturating_add(2);
    }
    hits
}

fn paint_rule(frame: &mut Frame, row: Rect, theme: &crate::theme::Theme) {
    if row.width == 0 || row.height == 0 {
        return;
    }
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(
            "─".repeat(row.width as usize),
            Style::default().fg(theme.border.subtle),
        ))),
        row,
    );
}

/// The card's only shortcut row. It is built from structured [`Hint`]s and
/// uses the same packing as the geometry prediction.
pub fn paint_footer(frame: &mut Frame, state: &AppState, geom: &FrameGeom) {
    if geom.footer.height == 0 {
        return;
    }
    let mut area = inset_row(geom.footer);
    if area.width == 0 {
        return;
    }
    let reserve = activity_stop_reserve(state);
    if reserve > 0 && area.width > reserve + 8 {
        area.width -= reserve;
    }
    let resolved = hints(state);
    let rows = hint_rows(&resolved, area.width as usize, area.height as usize);
    let n = rows.len() as u16;
    let y0 = area.y + area.height.saturating_sub(n);
    for (i, row) in rows.iter().enumerate() {
        let text_width: usize = row.iter().map(|hint| hint_text_width(hint)).sum::<usize>()
            + UnicodeWidthStr::width(" · ") * row.len().saturating_sub(1);
        let mut spans: Vec<Span<'static>> = Vec::new();
        for (index, hint) in row.iter().enumerate() {
            if index > 0 {
                spans.push(Span::styled(
                    " · ",
                    Style::default().fg(state.theme.text.muted),
                ));
            }
            spans.push(Span::styled(
                hint.key.clone(),
                if hint.action == HintAction::Info {
                    Style::default().fg(state.theme.text.muted)
                } else {
                    Style::default()
                        .fg(state.theme.text.primary)
                        .add_modifier(Modifier::BOLD)
                },
            ));
            if !hint.label.is_empty() {
                spans.push(Span::raw(" "));
                spans.push(Span::styled(
                    hint.label.clone(),
                    Style::default().fg(state.theme.text.muted),
                ));
            }
        }
        let width = (text_width as u16).min(area.width);
        let x = area.x + area.width.saturating_sub(width) / 2;
        frame.render_widget(
            Paragraph::new(Line::from(spans)).alignment(ratatui::layout::Alignment::Left),
            Rect {
                x,
                y: y0 + i as u16,
                width,
                height: 1,
            },
        );
    }
}

/// Columns the running-task stop button needs on the card footer.
fn activity_stop_reserve(state: &AppState) -> u16 {
    if state.active_screen != Screen::Activity {
        return 0;
    }
    let Some(crate::activity::ActivityId::Background(id)) = state.activity_open.as_ref() else {
        return 0;
    };
    let running = state
        .background_task_labels
        .get(id)
        .is_some_and(|task| task.is_running());
    if !running {
        return 0;
    }
    UnicodeWidthStr::width(state.t().background_stop_button) as u16 + 2
}

fn inset_row(area: Rect) -> Rect {
    let pad = crate::secondary::card_gutter(area.width);
    crate::layout::horizontal_inset(area, pad)
}

const CLOSE: &str = "[×]";

#[cfg(test)]
mod tests {
    use super::*;

    fn area(w: u16, h: u16) -> Rect {
        Rect {
            x: 0,
            y: 0,
            width: w,
            height: h,
        }
    }

    #[test]
    fn a_small_terminal_uses_the_whole_area() {
        let geom = frame_geom(area(40, 12), Screen::Skills);
        assert_eq!(geom.popup, area(40, 12));
        assert!(geom.content.width > 0 && geom.content.height > 0);
        assert!(geom.content.x >= geom.popup.x);
        assert!(geom.content.y + geom.content.height <= geom.popup.y + geom.popup.height);
    }

    #[test]
    fn a_short_terminal_drops_the_border_so_a_page_keeps_its_action_row() {
        let geom = frame_geom(area(80, 8), Screen::ActivityList);
        assert_eq!(geom.content, geom.popup);
        assert_eq!(geom.popup, area(80, 8));
        let plan = frame_geom(area(70, 10), Screen::Plan);
        assert_eq!(plan.content, plan.popup, "a 10-row plan keeps its steps");
        let taller = frame_geom(area(80, 12), Screen::ActivityList);
        assert!(taller.content.height < taller.popup.height);
    }

    #[test]
    fn a_regular_popup_is_centered_and_narrower_than_the_terminal() {
        let term = area(120, 40);
        let geom = frame_geom(term, Screen::Skills);
        assert!(geom.popup.width < term.width);
        assert!(geom.popup.height < term.height);
        assert_eq!(
            geom.popup.x,
            (term.width - geom.popup.width) / 2,
            "centered"
        );
        assert_eq!(geom.tab_row.height, 1);
        assert_eq!(geom.popup.width, 84, "70% of 120 for a List page");
        assert_eq!(geom.popup.height, 32, "List height cap");
        let short = frame_geom(area(80, 24), Screen::Skills);
        assert_eq!(short.popup.width, 56, "70% of 80");
        assert_eq!(short.popup.height, 16, "24 rows minus two 4-row margins");
        assert_eq!(geom.footer.height, 2, "grouped pages reserve two hint rows");
        assert_eq!(geom.title_row.y, geom.popup.y, "title sits on the border");
        assert!(geom.rule_row.y >= geom.tab_row.y);
        assert!(geom.content.y >= geom.rule_row.y + 1);
        assert!(geom.content.y + geom.content.height <= geom.footer.y);
        assert!(geom.footer.y + geom.footer.height <= geom.popup.y + geom.popup.height);
    }

    #[test]
    fn each_page_class_gets_its_own_popup() {
        let term = area(160, 50);
        let list = frame_geom(term, Screen::Skills);
        let list_detail = frame_geom(term, Screen::Settings);
        let document = frame_geom(term, Screen::Plan);
        let split = frame_geom(term, Screen::Diff);
        assert_eq!(list.popup.width, 110, "List max width");
        assert_eq!(list.popup.height, 32, "List max height");
        assert_eq!(list_detail.popup.width, 120, "ListDetail max width");
        assert_eq!(
            list_detail.popup.height, 48,
            "ListDetail uses its vertical share"
        );
        assert_eq!(document.popup.width, 140, "Document max width");
        assert_eq!(
            document.popup.height, 44,
            "Document uses its vertical share"
        );
        assert_eq!(split.popup.width, 120, "Split max width");
        assert_eq!(split.popup.height, 48, "Split uses its vertical share");
        assert!(list.popup.width < document.popup.width);
        assert_ne!(
            list.popup, list_detail.popup,
            "a list is not a two-pane page"
        );
    }

    /// Agents is a named card: the name, the sibling tabs and `[×]`, and no
    /// second `←` row. Context is the peer-tab card: the tabs are the name.
    #[test]
    fn grouped_pages_share_chrome_but_keep_their_content_class() {
        let agents = card_text(Screen::Agents);
        assert!(agents.contains("[×]"), "{agents}");
        assert!(agents.contains("扩展"), "{agents}");
        assert!(agents.contains("Agents"), "{agents}");
        assert!(
            !agents.contains("← "),
            "the agents card repeats its name under the tabs:\n{agents}"
        );
        let context = card_text(Screen::Context);
        assert!(context.contains("[×]"), "{context}");
        assert!(context.contains("检查"), "{context}");
        assert!(context.contains("差异"), "{context}");
        assert!(context.contains("上下文"), "{context}");
        assert!(
            !context.contains("← "),
            "the context card still paints a back title:\n{context}"
        );
        assert_ne!(
            frame_geom(area(160, 50), Screen::Agents).popup,
            frame_geom(area(160, 50), Screen::Context).popup,
            "List and Document are different content classes"
        );
    }

    /// Background and plan used to be full pages. Both open in the one host;
    /// Plan is a Runtime document, the background console is a Split page.
    #[test]
    fn background_and_plan_open_in_the_shared_host() {
        let term = area(120, 40);
        let plan = frame_geom(term, Screen::Plan);
        let jobs = frame_geom(term, Screen::ActivityList);
        assert_eq!(
            plan.popup.width, 108,
            "Plan is a Document, not a narrow list"
        );
        assert_eq!(jobs.popup.width, 108, "Background is a Split page");
        assert_ne!(plan.popup, jobs.popup);
        assert_eq!(plan.tab_row.height, 1, "plan is a tab, not its own page");
        assert_eq!(jobs.title_row.height, 1, "the jobs list has the card title");
        assert_eq!(jobs.tab_row.height, 0, "the jobs list is a single page");
        let text = card_text(Screen::ActivityList);
        assert!(text.contains("后台任务"), "{text}");
        assert!(text.contains("[×]"), "{text}");
        assert!(
            !text.contains("← "),
            "the jobs list is still a full page:\n{text}"
        );
        let plan_text = card_text(Screen::Plan);
        assert!(plan_text.contains("运行时"), "{plan_text}");
        assert!(plan_text.contains("[计划]"), "{plan_text}");
        assert!(plan_text.contains("[×]"), "{plan_text}");
        assert!(!plan_text.contains("← "), "{plan_text}");
    }

    /// Legacy page footers separate clauses with line breaks, ` · `, or two
    /// spaces. A single-clause parse would treat the whole Trace footer as a
    /// back clause and silently drop its page keys.
    #[test]
    fn every_legacy_hint_separator_keeps_its_page_keys() {
        let mut state = hint_state(crate::i18n::Locale::Zh);
        state.active_screen = Screen::Trace;
        state.card_hint = Some(state.t().trace_hint.to_string());
        let hints = hints(&state);
        for key in ["1-6", "Tab", "↑↓", "Enter", "f"] {
            assert!(
                hints.iter().any(|hint| hint.key == key),
                "Trace lost its {key} hint: {hints:?}"
            );
        }
        assert_eq!(
            hints.last().map(|hint| hint.action),
            Some(HintAction::Back),
            "back stays last"
        );
        assert!(page_hint_clauses("a  b   c").contains(&"c"));
    }

    /// A page-level mode owns the title even when the page normally lives under
    /// a named group. The tab still identifies the page.
    #[test]
    fn a_form_or_help_detail_titles_its_own_subject() {
        let mut state = hint_state(crate::i18n::Locale::Zh);
        state.active_screen = Screen::Help;
        state.help.detail = Some("/review");
        assert_eq!(card_title(&state), "/review");

        crate::schedules::open(&mut state);
        let effects = crate::reducer::reduce(
            &mut state,
            crate::action::Action::Key(crossterm::event::KeyEvent::new(
                crossterm::event::KeyCode::Char('n'),
                crossterm::event::KeyModifiers::NONE,
            )),
        );
        assert!(effects.is_empty(), "opening a form starts no work");
        assert_eq!(card_title(&state), state.t().schedules.form_create);
    }

    fn card_text(screen: Screen) -> String {
        let mut state = hint_state(crate::i18n::Locale::Zh);
        state.active_screen = screen;
        let backend = ratatui::backend::TestBackend::new(120, 40);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| crate::render::render(frame, &mut state))
            .unwrap();
        let buf = terminal.backend().buffer();
        let mut out = String::new();
        for y in 0..buf.area.height {
            let mut x = 0;
            while x < buf.area.width {
                let sym = buf.cell((x, y)).map(|cell| cell.symbol()).unwrap_or(" ");
                out.push_str(sym);
                x += UnicodeWidthStr::width(sym).max(1) as u16;
            }
            out.push('\n');
        }
        out
    }

    #[test]
    fn tab_steps_wrap_inside_the_group() {
        let siblings = tabs(Screen::Skills);
        assert_eq!(step(siblings, Screen::Skills, true), Screen::Mcp);
        assert_eq!(step(siblings, Screen::Skills, false), Screen::Agents);
        assert_eq!(step(siblings, Screen::Trust, true), Screen::Agents);
    }

    #[test]
    fn review_is_a_sibling_of_diff_and_not_a_command() {
        let siblings = tabs(Screen::Diff);
        assert_eq!(step(siblings, Screen::Diff, true), Screen::Review);
        assert!(siblings.contains(&Screen::Review));
    }

    #[test]
    fn pages_that_already_use_tab_keep_it() {
        assert!(page_uses_tab(Screen::Tasks));
        assert!(page_uses_tab(Screen::Context));
        assert!(page_uses_tab(Screen::Mcp));
        assert!(!page_uses_tab(Screen::Skills));
        assert!(!page_uses_tab(Screen::Diff));
        assert!(!page_uses_tab(Screen::Plan));
    }

    #[test]
    fn memory_stays_its_own_overlay() {
        assert!(!hosts(Screen::Memory));
        assert!(hosts(Screen::Skills));
        assert!(tabs(Screen::Memory).is_empty());
    }

    /// One shortcut block, centered. The page keys and the tab key share it.
    /// A second hint under the body was the stray line.
    #[test]
    fn a_hosted_card_has_one_centered_shortcut_block() {
        let mut state = hint_state(crate::i18n::Locale::Zh);
        state.active_screen = Screen::Agents;
        let backend = ratatui::backend::TestBackend::new(120, 40);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| crate::render::render(frame, &mut state))
            .unwrap();
        let buf = terminal.backend().buffer().clone();
        let geom = frame_geom(area(120, 40), Screen::Agents);
        let mut hits = 0u16;
        for y in geom.footer.y..geom.footer.y + geom.footer.height {
            let row = row_text(&buf, y);
            if row.contains("返回") {
                hits += 1;
                let mid = line_midpoint(&buf, "返回").expect("back clause");
                let center = geom.popup.x + geom.popup.width / 2;
                assert!(
                    mid.abs_diff(center) <= 2,
                    "shortcut midpoint {mid} is off the card center {center}: {row}"
                );
            }
        }
        assert_eq!(hits, 1, "the back clause belongs on one footer row");
        let card = card_text(Screen::Agents);
        assert!(card.contains("j/k"), "{card}");
        assert!(card.contains("Tab"), "{card}");
        assert!(
            !card.contains("点击标签"),
            "the mouse clause is a second footer:\n{card}"
        );
    }

    #[test]
    fn the_usage_heading_does_not_repeat_the_footer_keys() {
        let card = card_text(Screen::Context);
        let heading = card
            .lines()
            .find(|line| line.contains("模型用量"))
            .unwrap_or("");
        assert!(
            !heading.contains("Tab"),
            "the scope keys sit on the heading: {heading}"
        );
        assert!(card.contains("返回"), "{card}");
    }

    /// Midpoint of the hint block that contains `needle`.
    ///
    /// The block is the run of text around the needle, bounded by a border or
    /// by two spaces. Centering is two blocks sharing a midpoint, not two
    /// strings sharing a start column.
    fn line_midpoint(buf: &ratatui::buffer::Buffer, needle: &str) -> Option<u16> {
        let area = buf.area;
        for y in 0..area.height {
            let mut cells: Vec<(u16, String)> = Vec::new();
            let mut row = String::new();
            let mut x = 0u16;
            while x < area.width {
                let sym = buf
                    .cell((x, y))
                    .map(|cell| cell.symbol().to_string())
                    .unwrap_or_else(|| " ".into());
                let width = UnicodeWidthStr::width(sym.as_str()).max(1) as u16;
                row.push_str(&sym);
                cells.push((x, sym));
                x += width;
            }
            let Some(byte) = row.find(needle) else {
                continue;
            };
            let col = UnicodeWidthStr::width(&row[..byte]) as u16;
            let idx = cells.iter().position(|(start, _)| *start == col)?;
            let gap = |sym: &str| matches!(sym, "│" | "┌" | "┐" | "└" | "┘" | "├" | "┤" | "─");
            let mut left = idx;
            let mut blanks = 0u8;
            while left > 0 {
                let sym = cells[left - 1].1.as_str();
                if gap(sym) {
                    break;
                }
                if sym.trim().is_empty() {
                    blanks += 1;
                    if blanks >= 2 {
                        break;
                    }
                } else {
                    blanks = 0;
                }
                left -= 1;
            }
            while left < cells.len() && cells[left].1.trim().is_empty() {
                left += 1;
            }
            let mut right = idx;
            blanks = 0;
            while right + 1 < cells.len() {
                let sym = cells[right + 1].1.as_str();
                if gap(sym) {
                    break;
                }
                if sym.trim().is_empty() {
                    blanks += 1;
                    if blanks >= 2 {
                        break;
                    }
                } else {
                    blanks = 0;
                }
                right += 1;
            }
            while right > left && cells[right].1.trim().is_empty() {
                right -= 1;
            }
            let start = cells[left].0;
            let end =
                cells[right].0 + UnicodeWidthStr::width(cells[right].1.as_str()).max(1) as u16;
            return Some(start + end.saturating_sub(start) / 2);
        }
        None
    }

    /// 40 columns is a full-bleed popup. The workspace hint has to stay a
    /// complete phrase: the 40×12 plan frame used to end at `Esc` and drop
    /// `返回`.
    #[test]
    fn a_forty_column_workspace_hint_keeps_the_back_word() {
        for (locale, back) in [
            (crate::i18n::Locale::Zh, "返回"),
            (crate::i18n::Locale::En, "back"),
        ] {
            let mut state = hint_state(locale);
            state.active_screen = Screen::Skills;
            let geom = frame_geom(area(40, 12), Screen::Skills);
            let shown = footer_text(&mut state, 40, 12, geom.footer);
            assert!(
                shown.contains(back),
                "{locale:?} hint dropped the back word: {shown:?}"
            );
            for row in shown.lines() {
                assert!(
                    UnicodeWidthStr::width(row.trim_end()) <= geom.footer.width as usize,
                    "{locale:?} hint is wider than the footer: {row:?}"
                );
            }
        }
    }

    #[test]
    fn a_wide_footer_shows_the_page_keys_and_the_tab_key() {
        let mut state = hint_state(crate::i18n::Locale::Zh);
        state.active_screen = Screen::Skills;
        let geom = frame_geom(area(120, 40), Screen::Skills);
        let shown = footer_text(&mut state, 120, 40, geom.footer);
        assert!(
            shown.contains("Tab"),
            "wide hint dropped the tab key: {shown:?}"
        );
        assert!(
            shown.contains("Enter"),
            "wide hint dropped the page keys: {shown:?}"
        );
    }

    #[test]
    fn a_narrow_footer_paints_a_whole_phrase() {
        let mut state = hint_state(crate::i18n::Locale::En);
        state.active_screen = Screen::Mcp;
        let geom = frame_geom(area(20, 12), Screen::Mcp);
        let shown = footer_text(&mut state, 20, 12, geom.footer);
        assert!(
            shown.to_ascii_lowercase().contains("back"),
            "20-col hint dropped the back word: {shown:?}"
        );
        for row in shown.lines() {
            assert!(
                UnicodeWidthStr::width(row.trim_end()) <= geom.footer.width as usize,
                "20-col hint is wider than the footer: {row:?}"
            );
        }
    }

    fn rendered_rows(state: &mut AppState, w: u16, h: u16) -> Vec<String> {
        state.size = (w, h);
        let backend = ratatui::backend::TestBackend::new(w, h);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| crate::render::render(frame, state))
            .unwrap();
        let buf = terminal.backend().buffer();
        (0..h).map(|y| row_text(buf, y)).collect()
    }

    #[test]
    fn a_single_page_never_advertises_a_tab_switch() {
        let mut state = hint_state(crate::i18n::Locale::Zh);
        state.active_screen = Screen::Sessions;
        state.card_hint = Some("↑↓ 选择 · Enter 打开 · Esc 返回".into());
        let hints = hints(&state);
        assert!(
            !hints.iter().any(|hint| is_switch_clause(&hint.key)),
            "single page advertised a sibling switch: {hints:?}"
        );
        let geom = frame_geom_for(&state, area(120, 40));
        assert_eq!(geom.tab_row.height, 0);
    }

    #[test]
    fn a_grouped_page_advertises_one_real_switch() {
        let mut state = hint_state(crate::i18n::Locale::Zh);
        state.active_screen = Screen::Skills;
        let hints = hints(&state);
        let switches: Vec<&Hint> = hints
            .iter()
            .filter(|hint| is_switch_clause(&hint.key))
            .collect();
        assert_eq!(switches.len(), 1, "{hints:?}");
        assert_eq!(switches[0].key, "Tab");
        assert_eq!(switches[0].action, HintAction::SwitchTab);
    }

    #[test]
    fn a_page_key_that_mentions_tab_is_not_mistaken_for_a_switch() {
        let mut state = hint_state(crate::i18n::Locale::En);
        state.active_screen = Screen::Conversation;
        state.card_hint = Some("Enter/Tab view output · x stop · Esc back".into());
        let hints = hints(&state);
        assert!(
            hints.iter().any(|hint| hint.key == "Enter/Tab"),
            "a page action mentioning Tab was dropped: {hints:?}"
        );
        assert!(
            !hints
                .iter()
                .any(|hint| hint.action == HintAction::SwitchTab && hint.key == "Tab"),
            "a single page invented a switch: {hints:?}"
        );
    }

    #[test]
    fn a_two_line_schedule_hint_is_not_concatenated() {
        let mut state = hint_state(crate::i18n::Locale::Zh);
        state.active_screen = Screen::Schedules;
        state.card_hint = Some(state.t().schedules.footer_list.to_string());
        let hints = hints(&state);
        let texts: Vec<String> = hints.iter().map(Hint::text).collect();
        assert!(
            texts.iter().any(|text| text.contains("f 筛选")),
            "{texts:?}"
        );
        assert!(
            texts.iter().any(|text| text.contains("r 立即运行")),
            "{texts:?}"
        );
        assert!(
            !texts.iter().any(|text| text.contains("f 筛选r 立即运行")),
            "the newline was flattened: {texts:?}"
        );
    }

    #[test]
    fn the_scrim_erases_every_workbench_glyph() {
        let mut state = hint_state(crate::i18n::Locale::Zh);
        state.theme = crate::theme::Theme::dark();
        state.transcript.push_user("SECRET-BEHIND-CARD".into());
        state.active_screen = Screen::Plan;
        state.size = (120, 40);
        let geom = frame_geom_for(&state, area(120, 40));
        let backend = ratatui::backend::TestBackend::new(120, 40);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| crate::render::render(frame, &mut state))
            .unwrap();
        let buf = terminal.backend().buffer();
        for y in 0..40 {
            for x in 0..120 {
                if !geom.popup.contains(ratatui::layout::Position { x, y }) {
                    let cell = buf.cell((x, y)).expect("cell");
                    assert_eq!(
                        cell.symbol(),
                        " ",
                        "glyph leaked outside the card at ({x},{y})"
                    );
                    assert_eq!(
                        cell.style().bg,
                        Some(state.theme.surface.scrim),
                        "background leaked outside the card at ({x},{y})"
                    );
                }
            }
        }
        let all = (0..40)
            .map(|y| row_text(buf, y))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(!all.contains("SECRET-BEHIND-CARD"));
    }

    #[test]
    fn closing_a_page_preserves_the_workbench_state() {
        let mut state = hint_state(crate::i18n::Locale::Zh);
        state.size = (120, 40);
        state.composer.insert_str("draft-secret");
        state.conv.scroll = 7;
        state.conv.auto_scroll = false;
        state.workbench_focus = crate::state::WorkbenchFocus::Conversation;
        let draft = state.composer.text().to_string();
        let scroll = state.conv.scroll;
        let focus = state.workbench_focus;
        state.active_screen = Screen::Plan;
        let _ = rendered_rows(&mut state, 120, 40);
        assert_eq!(state.composer.text(), draft);
        assert_eq!(state.conv.scroll, scroll);
        assert_eq!(state.workbench_focus, focus);
        state.active_screen = Screen::Conversation;
        let rows = rendered_rows(&mut state, 120, 40);
        assert_eq!(state.composer.text(), draft);
        assert_eq!(state.conv.scroll, scroll);
        assert_eq!(state.workbench_focus, focus);
        assert!(
            rows.join("\n").contains("draft-secret"),
            "the draft was lost after closing the page"
        );
    }

    #[test]
    fn a_tab_click_switches_to_that_tab_without_closing_the_page() {
        let mut state = hint_state(crate::i18n::Locale::Zh);
        state.active_screen = Screen::Plan;
        let _ = rendered_rows(&mut state, 120, 40);
        let (rect, target) = state
            .workspace_hits
            .tabs
            .iter()
            .find(|(_, screen)| *screen == Screen::Goal)
            .cloned()
            .expect("goal tab hit");
        let effects = chrome_mouse(
            &mut state,
            crossterm::event::MouseEvent {
                kind: crossterm::event::MouseEventKind::Down(crossterm::event::MouseButton::Left),
                column: rect.x + 1,
                row: rect.y,
                modifiers: crossterm::event::KeyModifiers::empty(),
            },
        );
        assert!(effects.is_some());
        assert_eq!(state.active_screen, target);
        assert_eq!(state.active_screen, Screen::Goal);
    }

    #[test]
    fn opening_and_closing_a_page_leaves_the_conversation_pixels_unchanged() {
        let mut state = hint_state(crate::i18n::Locale::Zh);
        state.size = (120, 40);
        state.composer.insert_str("draft-pixels");
        state.conv.scroll = 3;
        state.conv.auto_scroll = false;
        state.workbench_focus = crate::state::WorkbenchFocus::Conversation;
        let before = rendered_rows(&mut state, 120, 40);
        state.active_screen = Screen::Plan;
        let _ = rendered_rows(&mut state, 120, 40);
        state.active_screen = Screen::Conversation;
        let after = rendered_rows(&mut state, 120, 40);
        // The clock lives on the bottom row; the conversation body must be
        // byte-identical after the modal closes.
        assert_eq!(
            before[4..37],
            after[4..37],
            "the modal changed the workbench render"
        );
        assert!(after[4..37].iter().any(|row| row.contains("draft-pixels")));
    }

    #[test]
    fn closing_a_page_does_not_stop_a_running_background_task() {
        let mut state = hint_state(crate::i18n::Locale::Zh);
        state.background_task_labels.insert(
            "bg-1".into(),
            crate::state::BackgroundTaskChrome::running("sleep 300", 0),
        );
        state.active_screen = Screen::ActivityList;
        let _ = rendered_rows(&mut state, 120, 40);
        let _ = crate::reducer::screen_nav::back(&mut state);
        assert_eq!(state.active_screen, Screen::Conversation);
        assert!(
            state
                .background_task_labels
                .get("bg-1")
                .is_some_and(crate::state::BackgroundTaskChrome::is_running),
            "closing the observational page stopped the task"
        );
    }

    #[test]
    fn page_status_reflects_real_state_and_never_invents_a_value() {
        let mut state = hint_state(crate::i18n::Locale::Zh);
        state.active_screen = Screen::Tasks;
        state.tasks.index_pending = true;
        let loading = state.t().tasks.loading.to_string();
        let status = page_status(&state).expect("loading status");
        assert!(
            status.contains(&loading),
            "a pending task query must be visible: {status}"
        );
        state.tasks.index_pending = false;
        let idle = page_status(&state).unwrap_or_default();
        assert!(
            !idle.contains(&loading),
            "an idle task page must not claim it is loading: {idle}"
        );
        state.active_screen = Screen::Doctor;
        assert_eq!(
            page_status(&state),
            Some(state.t().doctor_page.never_checked.to_string())
        );
        state.active_screen = Screen::Team;
        assert_eq!(page_status(&state), None, "no children means no count");
    }

    #[test]
    fn hint_rows_keep_back_and_drop_page_keys_first() {
        let hints = vec![
            Hint {
                key: "a".into(),
                label: "very long page action that cannot fit".into(),
                priority: 10,
                action: HintAction::PageKey,
            },
            Hint {
                key: "Enter".into(),
                label: "another long page action".into(),
                priority: 20,
                action: HintAction::PageKey,
            },
            Hint {
                key: "Esc".into(),
                label: "返回".into(),
                priority: 255,
                action: HintAction::Back,
            },
        ];
        let rows = hint_rows(&hints, 24, 1);
        assert_eq!(rows.len(), 1);
        let text = rows[0]
            .iter()
            .map(|hint| hint.text())
            .collect::<Vec<_>>()
            .join(" · ");
        assert!(text.contains("Esc"), "{text}");
        assert!(!text.contains("very long"), "{text}");
    }

    #[test]
    fn resize_recomputes_the_card_and_content_rect() {
        let a = frame_geom(area(120, 40), Screen::Diff);
        let b = frame_geom(area(80, 24), Screen::Diff);
        assert_ne!(a.popup, b.popup);
        assert!(b.popup.width <= 80 && b.popup.height <= 24);
        for geom in [a, b] {
            assert!(geom.content.x >= geom.popup.x);
            assert!(geom.content.y >= geom.popup.y);
            assert!(geom.content.x + geom.content.width <= geom.popup.x + geom.popup.width);
            assert!(geom.content.y + geom.content.height <= geom.popup.y + geom.popup.height);
        }
    }

    #[test]
    fn the_host_publishes_close_tabs_and_hover() {
        let mut state = hint_state(crate::i18n::Locale::Zh);
        state.active_screen = Screen::Plan;
        let _ = rendered_rows(&mut state, 120, 40);
        let close = state.workspace_hits.close.expect("close hit");
        let tabs = state.workspace_hits.tabs.clone();
        assert!(!tabs.is_empty(), "runtime tabs are clickable");
        let point = ratatui::layout::Position {
            x: close.x + 1,
            y: close.y,
        };
        let effects = chrome_mouse(
            &mut state,
            crossterm::event::MouseEvent {
                kind: crossterm::event::MouseEventKind::Moved,
                column: point.x,
                row: point.y,
                modifiers: crossterm::event::KeyModifiers::empty(),
            },
        );
        assert!(
            effects.is_none(),
            "hover is published, but the page still owns the move"
        );
        assert_eq!(state.workspace_hover, Some(WorkspaceHover::Close));
        let _ = chrome_mouse(
            &mut state,
            crossterm::event::MouseEvent {
                kind: crossterm::event::MouseEventKind::Down(crossterm::event::MouseButton::Left),
                column: point.x,
                row: point.y,
                modifiers: crossterm::event::KeyModifiers::empty(),
            },
        );
        assert_eq!(state.active_screen, Screen::Conversation);
    }

    /// Pointer hover is a shared row fact: the page that published the hit
    /// owns it, and the key is the object's stable id.
    #[test]
    fn a_pointer_move_sets_hover_on_the_row_under_it() {
        let mut state = hint_state(crate::i18n::Locale::Zh);
        state.active_screen = Screen::Help;
        state.theme = crate::theme::Theme::dark();
        let _ = rendered_rows(&mut state, 100, 24);
        let (rect, key) = state
            .screen_rows
            .get(2)
            .cloned()
            .expect("help painted rows");
        let _ = crate::reducer::reduce(
            &mut state,
            crate::action::Action::Mouse(crossterm::event::MouseEvent {
                kind: crossterm::event::MouseEventKind::Moved,
                column: rect.x + 1,
                row: rect.y,
                modifiers: crossterm::event::KeyModifiers::empty(),
            }),
        );
        assert_eq!(
            state.list_hover,
            Some(crate::state::ListHover {
                screen: Screen::Help,
                key,
            })
        );
        let _ = crate::reducer::reduce(
            &mut state,
            crate::action::Action::Mouse(crossterm::event::MouseEvent {
                kind: crossterm::event::MouseEventKind::Moved,
                column: 0,
                row: 0,
                modifiers: crossterm::event::KeyModifiers::empty(),
            }),
        );
        assert_eq!(state.list_hover, None, "hover must clear off the row");
    }

    /// Geometry runs before the page paints. The footer source therefore has
    /// to be the current screen's typed state, never the previous frame's
    /// collected string.
    #[test]
    fn a_page_switch_reads_the_new_pages_hint_before_any_paint() {
        let mut state = hint_state(crate::i18n::Locale::Zh);
        state.active_screen = Screen::Schedules;
        state.card_hint = Some("Esc".into());
        let schedule = hints(&state);
        assert!(
            schedule.iter().any(|hint| hint.key == "n"),
            "Schedules must expose its own keys: {schedule:?}"
        );
        assert!(
            schedule.iter().any(|hint| hint.key == "Esc"),
            "back stays present"
        );

        state.active_screen = Screen::Skills;
        state.skills_page.searching = true;
        state.card_hint = Some("1-6".into());
        let search = hints(&state);
        assert!(
            search.iter().any(|hint| hint.key == "Enter/Esc"),
            "the search mode's own hint wins immediately: {search:?}"
        );
        assert!(
            !search.iter().any(|hint| hint.key == "1-6"),
            "the previous page's key must not survive one frame"
        );
        let geom = frame_geom_for(&state, area(120, 40));
        assert!(geom.footer.height >= 1);
    }

    fn hint_state(locale: crate::i18n::Locale) -> AppState {
        AppState::new(
            crate::theme::Theme::no_color(),
            crate::state::Boot {
                session_id: leveler_client_protocol::SessionId::new("s1"),
                user: "u".into(),
                version: "0".into(),
                show_welcome: false,
                draft_path: None,
                history_path: None,
                context_window: 0,
                locale,
                untrusted_config: Vec::new(),
                model_notice: None,
                thinking: None,
            },
        )
    }

    fn footer_text(state: &mut AppState, w: u16, h: u16, footer: Rect) -> String {
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;
        let backend = TestBackend::new(w, h);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| crate::render::render(frame, state))
            .unwrap();
        let buf = terminal.backend().buffer().clone();
        let mut lines = Vec::new();
        for y in footer.y..footer.y.saturating_add(footer.height) {
            let mut out = String::new();
            let mut x = footer.x;
            while x < footer.x + footer.width {
                let sym = buf.cell((x, y)).map(|c| c.symbol()).unwrap_or(" ");
                if !sym.is_empty() {
                    out.push_str(sym);
                }
                x += UnicodeWidthStr::width(sym).max(1) as u16;
            }
            if !out.trim().is_empty() {
                lines.push(out.trim_end().to_string());
            }
        }
        lines.join("\n")
    }

    fn row_text(buf: &ratatui::buffer::Buffer, y: u16) -> String {
        let mut out = String::new();
        let mut x = 0u16;
        while x < buf.area.width {
            let sym = buf.cell((x, y)).map(|c| c.symbol()).unwrap_or(" ");
            out.push_str(sym);
            x += UnicodeWidthStr::width(sym).max(1) as u16;
        }
        out
    }
}
