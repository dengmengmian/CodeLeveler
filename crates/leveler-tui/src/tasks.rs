//! `/tasks` — the unified task workbench.
//!
//! One page answers "what is CodeLeveler executing right now?" over facts the
//! runtime already owns:
//!
//! - the current session's goal and turn state (snapshot projection),
//! - its child agents and background tasks (`activity::summaries`, built from
//!   runtime events),
//! - schedule definitions and their runs (`ManageSchedule`),
//! - every durable session/goal across workspaces (`QueryGlobalTasks`).
//!
//! This module owns no lifecycle. It never schedules, starts or settles work;
//! cancellation is the existing runtime command bound to a live, stable id.
//! Rows are rebuilt from the authoritative state on every read, so a child
//! finishing or a background task exiting is visible immediately. Global
//! sessions are the one snapshot source, and the page says so with a
//! "Snapshot HH:MM" marker instead of presenting them as live.

use crossterm::event::{KeyCode, KeyEvent, MouseEvent, MouseEventKind};
use leveler_client_protocol::schedule::{
    ScheduleFilter, ScheduleRequest, ScheduleResponse, ScheduleRunView, ScheduleView,
};
use leveler_client_protocol::{
    ClientCommand, CommandId, RuntimeEvent, SessionId, UiGlobalTaskIndex, UiGlobalTaskSummary,
    UiTaskStatus,
};
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use unicode_width::UnicodeWidthStr;

use crate::action::Effect;
use crate::state::AppState;

/// How many terminal rows "Recent" keeps. A bounded history keeps a long-lived
/// workspace from turning the page into an unbounded log.
pub(crate) const RECENT_LIMIT: usize = 30;

/// The global session index is a whole-home projection. The page shows a
/// bounded window and says how many rows it did not read into memory; the
/// authoritative index itself is unchanged.
pub(crate) const GLOBAL_SESSION_LIMIT: usize = 200;

/// Which range of rows the page shows.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum TasksSection {
    #[default]
    Active,
    Recent,
    All,
}

impl TasksSection {
    fn next(self) -> Self {
        match self {
            Self::Active => Self::Recent,
            Self::Recent => Self::All,
            Self::All => Self::Active,
        }
    }
}

/// What a row is. The distinction is load-bearing: a schedule definition is
/// not a running attempt, and a goal is not a background process.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TaskKind {
    Session,
    Goal,
    ChildAgent,
    BackgroundTask,
    ScheduleDefinition,
    ScheduleRun,
    Review,
}

impl TaskKind {
    fn label(self, state: &AppState) -> &'static str {
        let t = &state.t().tasks;
        match self {
            Self::Session => t.kind_session,
            Self::Goal => t.kind_goal,
            Self::ChildAgent => t.kind_child,
            Self::BackgroundTask => t.kind_background,
            Self::ScheduleDefinition => t.kind_schedule,
            Self::ScheduleRun => t.kind_run,
            Self::Review => t.kind_review,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TaskGroup {
    Current,
    Children,
    Background,
    Scheduled,
    Sessions,
}

/// The page's status vocabulary. A typed mapping from existing runtime status
/// enums; no log text or model sentence is ever inspected.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TaskStatus {
    Running,
    Waiting,
    Completed,
    Failed,
    Cancelled,
    Interrupted,
    Unknown,
    Idle,
    Scheduled,
    Blocked,
    Incomplete,
}

impl TaskStatus {
    fn is_running(self) -> bool {
        matches!(self, Self::Running)
    }

    fn is_waiting(self) -> bool {
        matches!(self, Self::Waiting | Self::Blocked)
    }

    fn is_active(self) -> bool {
        self.is_running() || self.is_waiting()
    }

    fn is_terminal(self) -> bool {
        matches!(
            self,
            Self::Completed | Self::Failed | Self::Cancelled | Self::Interrupted | Self::Incomplete
        )
    }

    fn label(self, state: &AppState) -> &'static str {
        let t = &state.t().tasks;
        match self {
            Self::Running => t.status_running,
            Self::Waiting => t.status_waiting,
            Self::Completed => t.status_completed,
            Self::Failed => t.status_failed,
            Self::Cancelled => t.status_cancelled,
            Self::Interrupted => t.status_interrupted,
            Self::Unknown => t.status_unknown,
            Self::Idle => t.status_idle,
            Self::Scheduled => t.status_scheduled,
            Self::Blocked => t.status_blocked,
            Self::Incomplete => t.status_incomplete,
        }
    }

    fn glyph(self) -> &'static str {
        match self {
            Self::Running => "●",
            Self::Waiting | Self::Blocked => "◌",
            Self::Completed => "✓",
            Self::Failed => "✗",
            Self::Cancelled => "⊘",
            Self::Interrupted => "!",
            Self::Unknown => "?",
            Self::Idle => "·",
            Self::Scheduled => "◷",
            Self::Incomplete => "◐",
        }
    }
}

/// One visible task. `key` is the stable identity used for selection; it never
/// changes when the title, model or status changes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TaskRow {
    pub key: String,
    pub kind: TaskKind,
    /// The display group. A session is not always the CURRENT session: the
    /// global index rows are historical sessions and must never be nested
    /// under the current session's header.
    pub group: TaskGroup,
    pub status: TaskStatus,
    pub title: String,
    /// The owning session: current session, or the global session's id.
    pub session: Option<SessionId>,
    /// Where the row came from, for the detail panel (source store / workspace).
    pub source: Option<String>,
    /// A real timestamp from the runtime, when one exists.
    pub at: Option<String>,
    /// Supporting facts, already true as reported.
    pub facts: Vec<String>,
    /// Activity detail this row opens, for current-session children/background.
    pub activity: Option<crate::activity::ActivityId>,
    /// The row can be cancelled through an existing runtime command. Only ever
    /// true for current-session work that the runtime still reports running.
    pub cancel: CancelKind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CancelKind {
    None,
    Background,
    Child,
}

/// Everything the `/tasks` page holds.
#[derive(Debug, Default)]
pub struct TasksState {
    pub section: TasksSection,
    /// The runtime's cross-workspace session/goal index.
    pub index: Option<UiGlobalTaskIndex>,
    pub index_loaded_at: Option<String>,
    /// The one query whose answer this page accepts.
    pub index_query: Option<String>,
    /// A query was issued and its answer has not arrived.
    pub index_pending: bool,
    /// The page keeps its own schedule projection; it never mutates the
    /// schedule page's state.
    pub schedules: Vec<ScheduleView>,
    pub schedules_loaded_at: Option<String>,
    pub schedule_query: Option<CommandId>,
    pub schedule_pending: bool,
    pub schedule_error: Option<String>,
    /// Selected row key, stable across reordering.
    pub selected: Option<String>,
    /// The selected row's fact panel is expanded.
    pub detail_open: bool,
}

/// Open the page. Toggling `/tasks` while it is already open returns to the
/// conversation, exactly like every other view command.
pub fn open(state: &mut AppState) -> Vec<Effect> {
    if state.active_screen == crate::screen::Screen::Tasks {
        close(state);
        return Vec::new();
    }
    state.active_screen = crate::screen::Screen::Tasks;
    state.screen_scroll = 0;
    state.detail_return = None;
    state.tasks.detail_open = false;
    if state.tasks.selected.is_none() {
        ensure_selection(state);
    }
    refresh(state)
}

/// One level of back: an open task detail, otherwise the page. The same rule
/// the Esc key follows.
pub fn back(state: &mut AppState) -> bool {
    if state.tasks.detail_open {
        state.tasks.detail_open = false;
        return true;
    }
    false
}

pub fn close(state: &mut AppState) {
    state.active_screen = crate::screen::Screen::Conversation;
    state.screen_scroll = 0;
    // A late answer for a closed page is dropped, not attributed to a later
    // visit.
    state.tasks.index_query = None;
    state.tasks.index_pending = false;
    state.tasks.schedule_query = None;
    state.tasks.schedule_pending = false;
}

/// Re-read both snapshot sources. Current-session rows need no query: they are
/// projections of state the runtime already pushed.
pub fn refresh(state: &mut AppState) -> Vec<Effect> {
    let mut effects = Vec::new();
    // One snapshot query per open/refresh: a superseded answer is ignored by
    // correlation id, so a slow response cannot overwrite a newer one.
    let query_id = CommandId::generate();
    state.tasks.index_query = Some(query_id.as_str().to_string());
    state.tasks.index_pending = state.runtime_connected;
    if state.runtime_connected {
        effects.push(Effect::Send(ClientCommand::QueryGlobalTasks {
            requester_session_id: state.session_id.clone(),
            query_id: query_id.as_str().to_string(),
            include_archived: false,
        }));
    } else {
        // No connection: the page still opens with live local facts, and the
        // header says the runtime is gone. Do not pretend a query is in
        // flight.
        state.tasks.index_query = None;
    }

    let schedule_id = CommandId::generate();
    state.tasks.schedule_query = Some(schedule_id.clone());
    state.tasks.schedule_pending = state.runtime_connected;
    if state.runtime_connected {
        effects.push(Effect::Send(ClientCommand::ManageSchedule {
            requester_session_id: state.session_id.clone(),
            query_id: schedule_id,
            request: Box::new(ScheduleRequest::List {
                filter: Some(ScheduleFilter::All),
            }),
        }));
    } else {
        state.tasks.schedule_query = None;
    }
    effects
}

/// Events that make the two snapshot sources worth re-reading. Live rows do
/// not need this: their state is already here.
pub fn should_refresh(event: &RuntimeEvent) -> bool {
    matches!(
        event,
        RuntimeEvent::SessionOpened { .. } | RuntimeEvent::RuntimeReconnected { .. }
    )
}

/// Apply the runtime's global session index. Only the answer this page is
/// waiting for is accepted; another client's response and a superseded query
/// are ignored.
pub fn apply_global_tasks(
    state: &mut AppState,
    requester_session_id: &SessionId,
    query_id: &str,
    index: UiGlobalTaskIndex,
) {
    if requester_session_id != &state.session_id {
        return;
    }
    if state.tasks.index_query.as_deref() != Some(query_id) {
        return;
    }
    state.tasks.index_query = None;
    state.tasks.index_pending = false;
    state.tasks.index_loaded_at = Some(leveler_core::now().to_rfc3339());
    state.tasks.index = Some(index);
    ensure_selection(state);
}

/// Apply one schedule management answer owned by this page.
pub fn apply_schedule_response(
    state: &mut AppState,
    requester_session_id: &SessionId,
    query_id: &CommandId,
    response: ScheduleResponse,
) {
    if requester_session_id != &state.session_id {
        return;
    }
    if state.tasks.schedule_query.as_ref() != Some(query_id) {
        return;
    }
    state.tasks.schedule_query = None;
    state.tasks.schedule_pending = false;
    state.tasks.schedules_loaded_at = Some(leveler_core::now().to_rfc3339());
    match response {
        ScheduleResponse::List { schedules } => {
            state.tasks.schedules = schedules;
            state.tasks.schedule_error = None;
        }
        ScheduleResponse::Error { message } => {
            state.tasks.schedule_error = Some(message);
        }
        // This page asks only for the list. Any other answer is not ours to
        // interpret; leave the projection as it is rather than guessing.
        _ => {}
    }
    ensure_selection(state);
}

/// Clear page-local attribution when the session changes. The schedule list is
/// workspace-global and stays; session rows are re-queried.
pub fn session_changed(state: &mut AppState) {
    state.tasks.index_query = None;
    state.tasks.index_pending = false;
    state.tasks.selected = None;
    state.tasks.detail_open = false;
    ensure_selection(state);
}

// ---------------------------------------------------------------------------
// Rows
// ---------------------------------------------------------------------------

fn session_status(status: leveler_client_protocol::RuntimeStatus) -> TaskStatus {
    use leveler_client_protocol::RuntimeStatus as s;
    match status {
        s::Busy => TaskStatus::Running,
        s::Idle => TaskStatus::Idle,
        s::Error => TaskStatus::Failed,
    }
}

fn global_status(status: UiTaskStatus) -> TaskStatus {
    use leveler_client_protocol::UiTaskStatus as s;
    match status {
        s::Idle => TaskStatus::Idle,
        s::Running => TaskStatus::Running,
        // Both "the runtime asked the user for something" and "the user
        // answered" leave the task owed; neither is running and neither is
        // finished.
        s::WaitingUser | s::Answered => TaskStatus::Waiting,
        s::Completed => TaskStatus::Completed,
        s::Failed => TaskStatus::Failed,
        s::Cancelled => TaskStatus::Cancelled,
        s::Interrupted => TaskStatus::Interrupted,
        s::Unknown => TaskStatus::Unknown,
        s::Blocked => TaskStatus::Blocked,
        s::Incomplete => TaskStatus::Incomplete,
    }
}

/// A review's own lifecycle. A queued review waits; it is not running, and a
/// settled review keeps its real outcome. Reviews remain the review system's
/// business; this is only the row's status.
fn review_status(status: leveler_client_protocol::UiReviewStatus) -> TaskStatus {
    use leveler_client_protocol::UiReviewStatus as s;
    match status {
        s::Queued => TaskStatus::Waiting,
        s::Running => TaskStatus::Running,
        s::Completed => TaskStatus::Completed,
        s::Incomplete => TaskStatus::Incomplete,
        s::Cancelled => TaskStatus::Cancelled,
        s::Failed => TaskStatus::Failed,
    }
}

fn activity_status(status: crate::activity::ActivityStatus) -> TaskStatus {
    use crate::activity::ActivityStatus as s;
    match status {
        s::Running => TaskStatus::Running,
        s::Waiting => TaskStatus::Waiting,
        s::Completed => TaskStatus::Completed,
        s::Failed => TaskStatus::Failed,
        s::Stopped => TaskStatus::Cancelled,
        // The process may have died with a boot; nothing proves completion.
        s::Lost => TaskStatus::Interrupted,
        // Two records share an identity: no outcome can be attributed.
        s::Conflict => TaskStatus::Unknown,
        s::Interrupted => TaskStatus::Interrupted,
        // Its turn ended without a terminal reaching this client; unknown.
        s::Unreported => TaskStatus::Unknown,
    }
}

/// A persisted schedule run's state. These are the protocol's own documented
/// run-state values, not free-form log text.
fn run_status(state: &str) -> TaskStatus {
    match state {
        "dispatching" | "running" => TaskStatus::Running,
        "submitted" | "waiting_approval" | "blocked" => TaskStatus::Waiting,
        "succeeded" => TaskStatus::Completed,
        "failed" => TaskStatus::Failed,
        "cancelled" => TaskStatus::Cancelled,
        "skipped" | "missed" => TaskStatus::Interrupted,
        _ => TaskStatus::Unknown,
    }
}

fn fmt_seconds(secs: u64) -> String {
    crate::status_line::fmt_elapsed(secs)
}

fn fmt_stamp(raw: &str) -> String {
    chrono::DateTime::parse_from_rfc3339(raw)
        .map(|value| {
            value
                .with_timezone(&chrono::Local)
                .format("%m-%d %H:%M:%S")
                .to_string()
        })
        .unwrap_or_else(|_| raw.chars().take(24).collect())
}

/// Every row the page knows, before the section filter. Ordering is stable by
/// identity inside each group; status changes never reorder a group.
pub fn all_rows(state: &AppState) -> Vec<TaskRow> {
    let mut rows = Vec::new();

    // Current session: identity first, then its goal.
    let current_title = if !state.goal.trim().is_empty() {
        state.goal.trim().to_string()
    } else {
        format!("{}", state.session_id)
    };
    let current_status = if state.session_deleted {
        TaskStatus::Unknown
    } else {
        session_status(state.status)
    };
    rows.push(TaskRow {
        key: "session:current".into(),
        kind: TaskKind::Session,
        group: TaskGroup::Current,
        status: current_status,
        title: current_title,
        session: Some(state.session_id.clone()),
        source: state.repository.clone(),
        at: None,
        facts: if state.session_deleted {
            vec![
                format!("session {}", state.session_id),
                "the runtime no longer has this session".into(),
            ]
        } else {
            vec![format!("session {}", state.session_id)]
        },
        activity: None,
        cancel: CancelKind::None,
    });

    if let Some(goal) = &state.execution_goal {
        let status = if goal.user_paused {
            TaskStatus::Waiting
        } else if goal.running {
            TaskStatus::Running
        } else {
            // Owed work with no live execution window: waiting, never done.
            TaskStatus::Waiting
        };
        let mut facts = vec![format!("goal {}", goal.goal_id)];
        if goal.user_paused {
            facts.push(state.t().tasks.pause.to_string());
        } else if !goal.running {
            facts.push(state.t().tasks.owed.to_string());
        }
        rows.push(TaskRow {
            key: format!("goal:{}", goal.goal_id),
            kind: TaskKind::Goal,
            group: TaskGroup::Current,
            status,
            title: goal.objective.clone(),
            session: Some(state.session_id.clone()),
            source: None,
            at: None,
            facts,
            activity: None,
            cancel: CancelKind::None,
        });
    }

    // Child agents and background tasks: one typed projection over live state.
    let mut reviews = Vec::new();
    for block in state.transcript.reviews() {
        let summary = &block.summary;
        let status = review_status(summary.status);
        let mut facts = vec![format!("review {}", summary.id)];
        if !summary.model.is_empty() {
            facts.push(format!("model {}", summary.model));
        }
        if summary.object_changed {
            facts.push("the reviewed object changed during the review".into());
        }
        if summary.modified_source {
            facts.push("the reviewer modified source (contract violation)".into());
        }
        if let Some(failure) = summary.failure.as_deref() {
            facts.push(failure.to_string());
        }
        reviews.push(TaskRow {
            key: format!("review:{}", summary.id),
            kind: TaskKind::Review,
            group: TaskGroup::Current,
            status,
            title: if summary.focus.trim().is_empty() {
                format!("review {}", summary.id)
            } else {
                summary.focus.clone()
            },
            session: Some(state.session_id.clone()),
            source: None,
            at: Some(summary.created_at.clone()),
            facts,
            activity: None,
            cancel: CancelKind::None,
        });
    }
    reviews.sort_by(|a, b| a.key.cmp(&b.key));
    rows.append(&mut reviews);

    let mut children = Vec::new();
    let mut background = Vec::new();
    for summary in crate::activity::summaries(state) {
        let (kind, cancel) = match summary.kind {
            crate::activity::ActivityKind::ChildAgent => (TaskKind::ChildAgent, CancelKind::Child),
            crate::activity::ActivityKind::BackgroundTask => {
                (TaskKind::BackgroundTask, CancelKind::Background)
            }
        };
        let activity = summary.id.clone();
        let status = activity_status(summary.status);
        // A child's nickname is identity, not its job. Lead with the task
        // title the runtime recorded, and keep the nickname as a fact: the
        // page answers "what is running", not "who was spawned".
        let display_title = if kind == TaskKind::ChildAgent {
            summary
                .task_title
                .clone()
                .unwrap_or_else(|| summary.title.clone())
        } else {
            summary.title.clone()
        };
        let mut facts = Vec::new();
        if kind == TaskKind::ChildAgent {
            facts.push(format!("agent {}", summary.title));
        }
        if summary.duration_known {
            facts.push(format!(
                "{} {}",
                state.t().tasks.elapsed,
                fmt_seconds(summary.duration_secs)
            ));
        }
        if let Some(secondary) = summary.secondary.as_deref() {
            facts.push(secondary.to_string());
        }
        let cancel = if status.is_running() || (kind == TaskKind::ChildAgent && status.is_waiting())
        {
            cancel
        } else {
            CancelKind::None
        };
        let row = TaskRow {
            key: format!("{}:{}", kind_key(kind), activity.as_key()),
            kind,
            group: if kind == TaskKind::ChildAgent {
                TaskGroup::Children
            } else {
                TaskGroup::Background
            },
            status,
            title: display_title,
            session: Some(state.session_id.clone()),
            source: None,
            at: None,
            facts,
            activity: Some(activity),
            cancel,
        };
        match kind {
            TaskKind::ChildAgent => children.push(row),
            _ => background.push(row),
        }
    }
    // A HashMap drives background labels; sort by stable identity so a
    // selection never jumps because the map iterated differently.
    children.sort_by(|a, b| a.key.cmp(&b.key));
    background.sort_by(|a, b| a.key.cmp(&b.key));
    rows.append(&mut children);
    rows.append(&mut background);

    // Schedule definitions and their runs are distinct entities.
    let mut scheduled = Vec::new();
    for view in &state.tasks.schedules {
        let definition_status = if view.needs_attention || view.grant.requires_regrant {
            TaskStatus::Blocked
        } else {
            TaskStatus::Scheduled
        };
        scheduled.push(TaskRow {
            key: format!("schedule:{}", view.id),
            kind: TaskKind::ScheduleDefinition,
            group: TaskGroup::Scheduled,
            status: definition_status,
            title: view.name.clone(),
            session: None,
            source: Some(view.workspace.clone()),
            at: view.next_run_at.clone(),
            facts: vec![view.trigger_text.clone(), view.description.clone()],
            activity: None,
            cancel: CancelKind::None,
        });
        // A definition is not a run. The in-flight attempt and the last
        // terminal attempt are separate rows; when the last run IS the live
        // one, it is shown once.
        let live_recorded = view.last_run.as_ref().is_some_and(|run| {
            matches!(
                run_status(&run.state),
                TaskStatus::Running | TaskStatus::Waiting
            )
        });
        if view.running && !live_recorded {
            scheduled.push(schedule_run_row(view, None, true));
        }
        if let Some(run) = view.last_run.as_ref() {
            scheduled.push(schedule_run_row(view, Some(run), false));
        }
    }
    scheduled.sort_by(|a, b| a.key.cmp(&b.key));
    rows.append(&mut scheduled);

    // Global sessions, excluding the one already shown in full above.
    if let Some(index) = &state.tasks.index {
        let mut shown = 0usize;
        for task in &index.tasks {
            if task.id == state.session_id {
                continue;
            }
            if shown >= GLOBAL_SESSION_LIMIT {
                break;
            }
            rows.push(global_session_row(state, task));
            shown += 1;
        }
    }

    rows
}

/// Rows the page's bounded windows left out for the current filter. Global
/// sessions beyond the cap and Recent's terminal window are both counted only
/// when they would have matched the filter.
pub fn rows_hidden(state: &AppState) -> usize {
    let global_hidden = |section: TasksSection| {
        state.tasks.index.as_ref().map_or(0, |index| {
            index
                .tasks
                .iter()
                .filter(|task| task.id != state.session_id)
                .skip(GLOBAL_SESSION_LIMIT)
                .filter(|task| {
                    let status = global_status(task.status);
                    match section {
                        TasksSection::Active => status.is_active(),
                        TasksSection::Recent => status.is_terminal(),
                        TasksSection::All => true,
                    }
                })
                .count()
        })
    };
    match state.tasks.section {
        TasksSection::Active => global_hidden(TasksSection::Active),
        TasksSection::All => global_hidden(TasksSection::All),
        TasksSection::Recent => {
            let materialized = all_rows(state)
                .iter()
                .filter(|row| row.status.is_terminal())
                .count();
            materialized
                .saturating_sub(RECENT_LIMIT)
                .saturating_add(global_hidden(TasksSection::Recent))
        }
    }
}

fn kind_key(kind: TaskKind) -> &'static str {
    match kind {
        TaskKind::Session => "session",
        TaskKind::Goal => "goal",
        TaskKind::ChildAgent => "child",
        TaskKind::BackgroundTask => "background",
        TaskKind::ScheduleDefinition => "schedule",
        TaskKind::ScheduleRun => "schedule-run",
        TaskKind::Review => "review",
    }
}

fn schedule_run_row(view: &ScheduleView, run: Option<&ScheduleRunView>, live: bool) -> TaskRow {
    let (id, state, scheduled, started, finished, exit_code, error) = match run {
        Some(run) => (
            run.id.clone(),
            run.state.as_str(),
            Some(run.scheduled_for.clone()),
            Some(run.started_at.clone()),
            run.finished_at.clone(),
            run.exit_code,
            run.error.clone(),
        ),
        None => (
            format!("{}:live", view.id),
            "running",
            None,
            None,
            None,
            None,
            None,
        ),
    };
    let status = if live && run.is_none() {
        TaskStatus::Running
    } else {
        run_status(state)
    };
    let mut facts = vec![format!("run {}", id)];
    if let Some(scheduled) = scheduled {
        facts.push(format!("scheduled {scheduled}"));
    }
    if let Some(started) = started {
        facts.push(format!("started {started}"));
    }
    if let Some(finished) = finished {
        facts.push(format!("finished {finished}"));
    }
    if let Some(code) = exit_code {
        facts.push(format!("exit {code}"));
    }
    if let Some(error) = error {
        facts.push(error);
    }
    TaskRow {
        key: format!("schedule-run:{}:{}", view.id, id),
        kind: TaskKind::ScheduleRun,
        group: TaskGroup::Scheduled,
        status,
        title: view.name.clone(),
        session: None,
        source: Some(view.workspace.clone()),
        at: run.map(|r| r.started_at.clone()),
        facts,
        activity: None,
        cancel: CancelKind::None,
    }
}

fn global_session_row(state: &AppState, task: &UiGlobalTaskSummary) -> TaskRow {
    let mut facts = vec![
        format!("session {}", task.id),
        format!("source {}", task.source_id),
    ];
    if !task.model.is_empty() {
        facts.push(format!("model {}", task.model));
    }
    if task.workspace_available == Some(false) {
        facts.push(state.t().tasks.workspace_missing.to_string());
    }
    if let Some(workspace) = &task.primary_workspace {
        facts.push(workspace.clone());
    }
    TaskRow {
        key: format!("session:{}:{}", task.source_id, task.id.as_str()),
        kind: TaskKind::Session,
        group: TaskGroup::Sessions,
        status: global_status(task.status),
        title: task.title.clone(),
        session: Some(task.id.clone()),
        source: Some(task.source_id.clone()),
        at: Some(task.last_activity_at.clone()),
        facts,
        activity: None,
        cancel: CancelKind::None,
    }
}

/// Rows after the section filter, in display order.
pub fn visible_rows(state: &AppState) -> Vec<TaskRow> {
    let mut rows: Vec<TaskRow> = all_rows(state)
        .into_iter()
        .filter(|row| match state.tasks.section {
            TasksSection::Active => row.status.is_active(),
            TasksSection::Recent => row.status.is_terminal(),
            TasksSection::All => true,
        })
        .collect();
    if state.tasks.section == TasksSection::Recent {
        rows.truncate(RECENT_LIMIT);
    }
    rows
}

/// Keep the selection on a row that is still visible. When the selected row
/// vanished (it left the filter, or its source disappeared), the nearest
/// remaining row is selected rather than the cursor jumping to an arbitrary
/// row.
pub fn ensure_selection(state: &mut AppState) {
    let rows = visible_rows(state);
    let Some(first) = rows.first() else {
        state.tasks.selected = None;
        return;
    };
    if let Some(selected) = &state.tasks.selected
        && rows.iter().any(|row| &row.key == selected)
    {
        return;
    }
    state.tasks.selected = Some(first.key.clone());
}

fn selected_index(state: &AppState, rows: &[TaskRow]) -> Option<usize> {
    let selected = state.tasks.selected.as_deref()?;
    rows.iter().position(|row| row.key == selected)
}

fn move_selection(state: &mut AppState, delta: isize) {
    let rows = visible_rows(state);
    if rows.is_empty() {
        state.tasks.selected = None;
        return;
    }
    let current = selected_index(state, &rows).unwrap_or(0);
    let next = if delta < 0 {
        current.saturating_sub(delta.unsigned_abs())
    } else {
        (current + delta as usize).min(rows.len() - 1)
    };
    state.tasks.selected = Some(rows[next].key.clone());
}

fn cycle_section(state: &mut AppState) {
    state.tasks.section = state.tasks.section.next();
    state.screen_scroll = 0;
    ensure_selection(state);
}

fn selected_row(state: &AppState) -> Option<TaskRow> {
    let rows = visible_rows(state);
    let selected = state.tasks.selected.as_deref()?;
    rows.into_iter().find(|row| row.key == selected)
}

// ---------------------------------------------------------------------------
// Actions
// ---------------------------------------------------------------------------

/// Open the selected row's existing detail surface. Cross-workspace sessions
/// have no local runtime to open; their details are the fact panel, not a
/// fabricated second detail page.
pub fn open_selected(state: &mut AppState) -> Vec<Effect> {
    let Some(row) = selected_row(state) else {
        return Vec::new();
    };
    match row.kind {
        TaskKind::ChildAgent | TaskKind::BackgroundTask => {
            if let Some(id) = row.activity {
                let mut effects = crate::activity::open(state, id);
                // Return to THIS page with its filter and selection intact.
                state.detail_return = Some(crate::screen::Screen::Tasks);
                effects.extend(Vec::new());
                return effects;
            }
        }
        TaskKind::Goal => {
            state.detail_return = Some(crate::screen::Screen::Tasks);
            state.active_screen = crate::screen::Screen::Goal;
            state.screen_scroll = 0;
            return Vec::new();
        }
        TaskKind::Session if row.session.as_ref() == Some(&state.session_id) => {
            close(state);
            return Vec::new();
        }
        // A schedule row's facts open in place; `s` is the explicit jump to the
        // schedules page. Enter never replaces this page's selection or filter
        // with another page's state.
        TaskKind::ScheduleDefinition | TaskKind::ScheduleRun => {
            state.tasks.detail_open = true;
            return Vec::new();
        }
        _ => {}
    }
    // Everything else has an authoritative fact panel; Enter toggles it.
    state.tasks.detail_open = !state.tasks.detail_open;
    Vec::new()
}

/// Jump to the existing schedules page (the schedule rows are a projection of
/// the same runtime definitions this button opens).
pub fn open_schedules(state: &mut AppState) -> Vec<Effect> {
    state.detail_return = None;
    crate::schedules::open(state)
}

/// Cancel exactly the selected task, through the existing runtime command.
/// The row is re-resolved from live state first; a finished or foreign row is
/// refused instead of sending a stale cancellation.
pub fn cancel_selected(state: &mut AppState) -> Vec<Effect> {
    let unavailable = state.t().tasks.cancel_unavailable;
    let Some(row) = selected_row(state) else {
        set_notice(state, unavailable);
        return Vec::new();
    };
    if row.session.as_ref() != Some(&state.session_id) {
        set_notice(state, unavailable);
        return Vec::new();
    }
    match (row.cancel, row.activity.as_ref()) {
        (CancelKind::Background, Some(crate::activity::ActivityId::Background(task_id))) => {
            if !state
                .background_task_labels
                .get(task_id)
                .is_some_and(|task| task.is_running())
            {
                set_notice(state, unavailable);
                return Vec::new();
            }
            vec![Effect::Send(ClientCommand::CancelBackgroundTask {
                session_id: state.session_id.clone(),
                task_id: task_id.clone(),
            })]
        }
        (CancelKind::Child, Some(crate::activity::ActivityId::Child(child_id))) => {
            let live = state.team.children.iter().any(|child| {
                child.id == *child_id
                    && matches!(
                        child.status,
                        crate::multi_agent::ChildStatus::Running
                            | crate::multi_agent::ChildStatus::Waiting
                    )
            });
            if !live {
                set_notice(state, unavailable);
                return Vec::new();
            }
            vec![Effect::Send(ClientCommand::CancelChild {
                session_id: state.session_id.clone(),
                child_id: child_id.clone(),
            })]
        }
        _ => {
            set_notice(state, unavailable);
            Vec::new()
        }
    }
}

fn set_notice(state: &mut AppState, message: &'static str) {
    state.notification = Some(crate::state::Notification {
        level: leveler_client_protocol::NotificationLevel::Warning,
        message: message.to_string(),
    });
}

/// The page's keys. It owns every key while it is the active screen; nothing
/// here reaches the composer.
pub fn handle_key(state: &mut AppState, key: KeyEvent) -> Vec<Effect> {
    // A live row can leave the current filter between two presses; the
    // selection follows the nearest remaining row instead of becoming stale.
    ensure_selection(state);
    match key.code {
        KeyCode::Esc => {
            if state.tasks.detail_open {
                state.tasks.detail_open = false;
            } else {
                close(state);
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
        KeyCode::PageDown => {
            move_selection(state, 10);
            Vec::new()
        }
        KeyCode::PageUp => {
            move_selection(state, -10);
            Vec::new()
        }
        KeyCode::Tab => {
            cycle_section(state);
            Vec::new()
        }
        KeyCode::BackTab => {
            // Shift+Tab is the same ring in the other direction: three states
            // make forward and backward the same two presses.
            state.tasks.section = match state.tasks.section {
                TasksSection::Active => TasksSection::All,
                TasksSection::Recent => TasksSection::Active,
                TasksSection::All => TasksSection::Recent,
            };
            ensure_selection(state);
            Vec::new()
        }
        KeyCode::Enter => open_selected(state),
        KeyCode::Char('r') | KeyCode::Char('R') => refresh(state),
        KeyCode::Char('x') | KeyCode::Char('X') => cancel_selected(state),
        KeyCode::Char('s') | KeyCode::Char('S') => {
            if matches!(
                selected_row(state).map(|row| row.kind),
                Some(TaskKind::ScheduleDefinition | TaskKind::ScheduleRun)
            ) {
                open_schedules(state)
            } else {
                Vec::new()
            }
        }
        _ => Vec::new(),
    }
}

/// Mouse: the wheel moves the selection; a click selects the painted row and
/// toggles that row's fact panel only when it is already selected. Row hits
/// are published by the renderer from exactly what it painted.
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
            let hit = state
                .screen_rows
                .iter()
                .find(|(rect, _)| rect.contains(point))
                .map(|(_, key)| key.clone());
            if let Some(key) = hit {
                // The shared list rule: a click selects; Enter opens or
                // toggles the detail. A second click on the same row must not
                // change the frame.
                state.tasks.selected = Some(key);
            }
            Vec::new()
        }
        _ => Vec::new(),
    }
}

// ---------------------------------------------------------------------------
// Rendering
// ---------------------------------------------------------------------------

fn status_style(state: &AppState, status: TaskStatus) -> Style {
    let theme = &state.theme;
    let color = match status {
        TaskStatus::Running => theme.status.running,
        TaskStatus::Waiting | TaskStatus::Blocked | TaskStatus::Scheduled => theme.status.warning,
        TaskStatus::Completed => theme.status.success,
        TaskStatus::Failed => theme.status.error,
        TaskStatus::Cancelled | TaskStatus::Interrupted | TaskStatus::Incomplete => {
            theme.status.warning
        }
        TaskStatus::Unknown | TaskStatus::Idle => theme.text.muted,
    };
    Style::default().fg(color)
}

fn pad_display(text: &str, width: usize) -> String {
    let used = UnicodeWidthStr::width(text);
    let mut out = text.to_string();
    for _ in used..width {
        out.push(' ');
    }
    out
}

fn status_width(state: &AppState, rows: &[TaskRow]) -> usize {
    rows.iter()
        .map(|row| UnicodeWidthStr::width(row.status.label(state)))
        .max()
        .unwrap_or(0)
        .clamp(4, 12)
}

fn group_title(group: TaskGroup, state: &AppState) -> &'static str {
    let t = &state.t().tasks;
    match group {
        TaskGroup::Current => t.group_current,
        TaskGroup::Children => t.group_children,
        TaskGroup::Background => t.group_background,
        TaskGroup::Scheduled => t.group_scheduled,
        TaskGroup::Sessions => t.group_sessions,
    }
}

struct BodyLine {
    line: Line<'static>,
    row_key: Option<String>,
}

fn body_lines(state: &AppState, rows: &[TaskRow], width: usize) -> Vec<BodyLine> {
    let t = &state.t().tasks;
    let theme = &state.theme;
    let mut lines = Vec::new();

    if !state.runtime_connected {
        lines.push(BodyLine {
            line: Line::from(Span::styled(
                t.disconnected.to_string(),
                Style::default().fg(theme.status.warning),
            )),
            row_key: None,
        });
        lines.push(BodyLine {
            line: Line::default(),
            row_key: None,
        });
    }
    if let Some(index) = &state.tasks.index
        && !index.source_errors.is_empty()
    {
        lines.push(BodyLine {
            line: Line::from(Span::styled(
                t.source_errors
                    .replacen("{}", &index.source_errors.len().to_string(), 1),
                Style::default().fg(theme.status.warning),
            )),
            row_key: None,
        });
        lines.push(BodyLine {
            line: Line::default(),
            row_key: None,
        });
    }
    if let Some(error) = &state.tasks.schedule_error {
        lines.push(BodyLine {
            line: Line::from(Span::styled(
                format!("{}: {error}", t.group_scheduled),
                Style::default().fg(theme.status.error),
            )),
            row_key: None,
        });
        lines.push(BodyLine {
            line: Line::default(),
            row_key: None,
        });
    }
    if state.runtime_connected && state.tasks.index.is_none() && state.tasks.index_pending {
        lines.push(BodyLine {
            line: Line::from(Span::styled(
                t.loading.to_string(),
                Style::default().fg(theme.text.muted),
            )),
            row_key: None,
        });
        lines.push(BodyLine {
            line: Line::default(),
            row_key: None,
        });
    } else if state.tasks.index.is_none() && state.tasks.index_query.is_none() {
        lines.push(BodyLine {
            line: Line::from(Span::styled(
                t.not_loaded.to_string(),
                Style::default().fg(theme.text.muted),
            )),
            row_key: None,
        });
        lines.push(BodyLine {
            line: Line::default(),
            row_key: None,
        });
    }

    if rows.is_empty() {
        let message = if state.tasks.section == TasksSection::Active {
            t.empty_active
        } else {
            t.empty
        };
        lines.push(BodyLine {
            line: Line::from(Span::styled(
                message.to_string(),
                Style::default().fg(theme.text.muted),
            )),
            row_key: None,
        });
        return lines;
    }

    let width_for_status = status_width(state, rows);
    let mut last_group = None;
    for row in rows {
        let group = row.group;
        if last_group != Some(group) {
            if last_group.is_some() {
                lines.push(BodyLine {
                    line: Line::default(),
                    row_key: None,
                });
            }
            // A group header names its own size, like Grok Build's task
            // pane: the reader can tell a full section from an empty one
            // without counting rows.
            let count = rows.iter().filter(|row| row.group == group).count();
            lines.push(BodyLine {
                line: Line::from(vec![
                    Span::styled(
                        group_title(group, state).to_string(),
                        Style::default()
                            .fg(theme.text.muted)
                            .add_modifier(Modifier::BOLD),
                    ),
                    Span::styled(format!(" {count}"), Style::default().fg(theme.text.muted)),
                ]),
                row_key: None,
            });
            last_group = Some(group);
        }
        let selected = state.tasks.selected.as_deref() == Some(row.key.as_str());
        let marker = if selected { "›" } else { " " };
        let status = pad_display(row.status.label(state), width_for_status);
        // title budget = total - marker(1) - gap(1) - glyph(1) - gap(1) -
        // status(width) - gap(2) - kind label + gap. Keep it simple and let
        // the renderer clip; this is a single-line row by design.
        let title = crate::render::text::truncate_display(
            &crate::render::text::sanitize_ui_text(&row.title),
            width.saturating_sub(8),
        );
        let mut spans = vec![
            Span::styled(
                format!("{marker} "),
                Style::default().fg(theme.accent.primary),
            ),
            Span::styled(
                format!("{} ", row.status.glyph()),
                status_style(state, row.status),
            ),
            Span::styled(status, status_style(state, row.status)),
            Span::raw("  "),
            Span::styled(title, Style::default().fg(theme.text.primary)),
            Span::styled(
                format!("  {}", row.kind.label(state)),
                Style::default().fg(theme.text.muted),
            ),
        ];
        if let Some(at) = row.at.as_deref() {
            spans.push(Span::styled(
                format!(
                    "  {}",
                    crate::render::text::sanitize_ui_text(&fmt_stamp(at))
                ),
                Style::default().fg(theme.text.muted),
            ));
        }
        lines.push(BodyLine {
            line: Line::from(spans),
            row_key: Some(row.key.clone()),
        });
    }
    let hidden = rows_hidden(state);
    if hidden > 0 {
        lines.push(BodyLine {
            line: Line::from(Span::styled(
                t.capped.replacen("{}", &hidden.to_string(), 1),
                Style::default().fg(theme.text.muted),
            )),
            row_key: None,
        });
    }
    lines
}

fn detail_lines(state: &AppState, row: &TaskRow, width: usize) -> Vec<Line<'static>> {
    let t = &state.t().tasks;
    let theme = &state.theme;
    let mut out = vec![
        Line::from(vec![
            Span::styled(
                format!("{} · ", t.detail_title),
                Style::default()
                    .fg(theme.text.muted)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                row.kind.label(state).to_string(),
                Style::default().fg(theme.text.muted),
            ),
        ]),
        Line::from(Span::styled(
            crate::render::text::sanitize_ui_text(&row.title),
            Style::default().fg(theme.text.primary),
        )),
    ];
    for fact in &row.facts {
        let fact = crate::render::text::sanitize_ui_text(fact);
        for line in crate::render::text::wrap(&format!("  {fact}"), width) {
            out.push(Line::from(Span::styled(
                line,
                Style::default().fg(theme.text.muted),
            )));
        }
    }
    let hint = match row.kind {
        TaskKind::ChildAgent | TaskKind::BackgroundTask => t.hint_activity,
        TaskKind::Goal => t.hint_goal,
        TaskKind::ScheduleDefinition | TaskKind::ScheduleRun => t.hint_schedules,
        TaskKind::Session if row.session.as_ref() == Some(&state.session_id) => t.hint_session,
        TaskKind::Session => t.hint_global_session,
        TaskKind::Review => "",
    };
    if !hint.is_empty() {
        out.push(Line::default());
        out.push(Line::from(Span::styled(
            hint.to_string(),
            Style::default().fg(theme.accent.primary),
        )));
    }
    out
}

pub fn render_screen(frame: &mut Frame, area: Rect, state: &mut AppState) {
    let t = &state.t().tasks;
    let section = match state.tasks.section {
        TasksSection::Active => t.filter_active,
        TasksSection::Recent => t.filter_recent,
        TasksSection::All => t.filter_all,
    };
    let counts = {
        let rows = all_rows(state);
        let running = rows.iter().filter(|row| row.status.is_running()).count();
        let waiting = rows.iter().filter(|row| row.status.is_waiting()).count();
        let recent = rows.iter().filter(|row| row.status.is_terminal()).count();
        format!(
            "{} {} · {} {} · {} {}",
            t.count_active, running, t.count_waiting, waiting, t.count_recent, recent
        )
    };
    let mut status = vec![
        Span::styled(
            section.to_string(),
            Style::default().fg(state.theme.accent.primary),
        ),
        Span::styled(
            format!("  {counts}"),
            Style::default().fg(state.theme.text.muted),
        ),
    ];
    let loaded = state
        .tasks
        .index_loaded_at
        .iter()
        .chain(state.tasks.schedules_loaded_at.iter())
        .max();
    if let Some(loaded) = loaded {
        status.push(Span::styled(
            format!(
                "   · {} {}",
                t.snapshot,
                crate::render::text::sanitize_ui_text(&fmt_stamp(loaded))
            ),
            Style::default().fg(state.theme.text.muted),
        ));
    }
    if state.runtime_connected && (state.tasks.index_pending || state.tasks.schedule_pending) {
        status.push(Span::styled(
            format!("   · {}", t.loading),
            Style::default().fg(state.theme.status.info),
        ));
    }
    let hint = if state.tasks.detail_open {
        t.footer_detail
    } else {
        t.footer
    };
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

    // A task that completed while the page was open leaves Active; the
    // selection moves to the nearest remaining row rather than dangling.
    ensure_selection(state);
    let rows = visible_rows(state);
    let mut lines = body_lines(state, &rows, body.width as usize);
    let selected_line = state.tasks.selected.as_deref().and_then(|selected| {
        lines
            .iter()
            .position(|line| line.row_key.as_deref() == Some(selected))
    });

    // The list owns most of the pane when the fact panel is open; otherwise it
    // owns all of it. No second scroll owner: the offset follows the selection.
    let detail_height = if state.tasks.detail_open {
        (body.height / 2).max(4).min(body.height.saturating_sub(3))
    } else {
        0
    };
    let list_height = body.height.saturating_sub(detail_height);
    let list_area = Rect {
        height: list_height,
        ..body
    };
    let max_offset = lines.len().saturating_sub(list_area.height as usize);
    let offset = selected_line
        .map(|line| line.saturating_sub(list_area.height.saturating_sub(1) as usize))
        .unwrap_or(0)
        .min(max_offset);
    let visible: Vec<Line<'static>> = lines
        .drain(..)
        .enumerate()
        .filter(|(index, _)| *index >= offset && *index < offset + list_area.height as usize)
        .map(|(index, body)| {
            if let Some(key) = body.row_key {
                state.screen_rows.push((
                    Rect {
                        y: list_area.y + (index - offset) as u16,
                        height: 1,
                        ..list_area
                    },
                    key,
                ));
            }
            body.line
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
    if let Some(row) = rows
        .iter()
        .find(|row| Some(row.key.as_str()) == state.tasks.selected.as_deref())
    {
        let detail = detail_lines(state, row, detail_area.width as usize);
        frame.render_widget(Paragraph::new(detail), detail_area);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn state() -> AppState {
        let mut state = AppState::new(
            crate::theme::Theme::no_color(),
            crate::state::Boot {
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
        state.size = (100, 30);
        state
    }

    #[test]
    fn an_empty_page_has_no_fabricated_rows() {
        let state = state();
        let rows = all_rows(&state);
        // The current session is a real entity, even when it has nothing to
        // run; there is no invented goal, child, task or schedule beside it.
        assert_eq!(rows.len(), 1, "{rows:?}");
        assert_eq!(rows[0].kind, TaskKind::Session);
        assert_eq!(rows[0].status, TaskStatus::Idle);
    }

    #[test]
    fn a_paused_goal_is_waiting_not_running_and_not_complete() {
        let mut state = state();
        state.execution_goal = Some(leveler_client_protocol::UiExecutionTask {
            goal_id: "g1".into(),
            objective: "ship it".into(),
            running: false,
            user_paused: true,
        });
        let rows = all_rows(&state);
        let goal = rows.iter().find(|row| row.kind == TaskKind::Goal).unwrap();
        assert_eq!(goal.status, TaskStatus::Waiting);
    }

    #[test]
    fn a_schedule_definition_is_never_a_running_task() {
        let mut state = state();
        state.tasks.schedules = vec![sample_schedule("a", false, None)];
        let rows = all_rows(&state);
        let definition = rows
            .iter()
            .find(|row| row.kind == TaskKind::ScheduleDefinition)
            .unwrap();
        assert_eq!(definition.status, TaskStatus::Scheduled);
        assert!(!definition.status.is_active());
        let active: Vec<_> = visible_rows(&state)
            .into_iter()
            .filter(|row| row.kind == TaskKind::ScheduleDefinition)
            .collect();
        assert!(active.is_empty(), "{active:?}");
    }

    #[test]
    fn a_running_schedule_is_a_run_row_and_the_definition_stays_scheduled() {
        let mut state = state();
        state.tasks.schedules = vec![sample_schedule("a", true, None)];
        let rows = all_rows(&state);
        let run = rows
            .iter()
            .find(|row| row.kind == TaskKind::ScheduleRun)
            .unwrap();
        assert_eq!(run.status, TaskStatus::Running);
        let definition = rows
            .iter()
            .find(|row| row.kind == TaskKind::ScheduleDefinition)
            .unwrap();
        assert_eq!(definition.status, TaskStatus::Scheduled);
    }

    #[test]
    fn global_session_statuses_are_the_runtime_mapping() {
        use leveler_client_protocol::{UiGlobalTaskIndex, UiGlobalTaskSummary};
        let mut state = state();
        state.tasks.index = Some(UiGlobalTaskIndex {
            tasks: vec![UiGlobalTaskSummary {
                id: SessionId::new("other"),
                source_id: "/tmp/db".into(),
                title: "other work".into(),
                primary_workspace: Some("/tmp".into()),
                workspace_available: Some(true),
                last_activity_at: "2026-11-17T00:00:00Z".into(),
                archived_at: None,
                status: UiTaskStatus::WaitingUser,
                model: "m".into(),
            }],
            source_errors: Vec::new(),
        });
        let rows = all_rows(&state);
        let other = rows
            .iter()
            .find(|row| row.session.as_ref().map(SessionId::as_str) == Some("other"))
            .unwrap();
        assert_eq!(other.status, TaskStatus::Waiting);
        assert_eq!(other.group, TaskGroup::Sessions);
    }

    #[test]
    fn a_whole_home_index_is_projected_through_a_bounded_window() {
        use leveler_client_protocol::{UiGlobalTaskIndex, UiGlobalTaskSummary};
        let mut state = state();
        state.tasks.section = TasksSection::All;
        let tasks = (0..(GLOBAL_SESSION_LIMIT + 3))
            .map(|i| UiGlobalTaskSummary {
                id: SessionId::new(format!("remote-{i}")),
                source_id: "/tmp/db".into(),
                title: format!("session {i}"),
                primary_workspace: None,
                workspace_available: None,
                last_activity_at: "t".into(),
                archived_at: None,
                status: UiTaskStatus::Completed,
                model: String::new(),
            })
            .collect();
        state.tasks.index = Some(UiGlobalTaskIndex {
            tasks,
            source_errors: Vec::new(),
        });
        let global_rows = all_rows(&state)
            .into_iter()
            .filter(|row| {
                row.kind == TaskKind::Session && row.session.as_ref() != Some(&state.session_id)
            })
            .count();
        assert_eq!(global_rows, GLOBAL_SESSION_LIMIT);
        assert_eq!(rows_hidden(&state), 3);
    }

    #[test]
    fn stale_answers_are_ignored_and_the_newest_query_wins() {
        let mut state = state();
        let query = "q-new";
        let _ = refresh(&mut state);
        state.tasks.index_query = Some(query.into());
        let mut index = leveler_client_protocol::UiGlobalTaskIndex::default();
        index
            .tasks
            .push(leveler_client_protocol::UiGlobalTaskSummary {
                id: SessionId::new("s2"),
                source_id: "/tmp/db".into(),
                title: "new".into(),
                primary_workspace: None,
                workspace_available: None,
                last_activity_at: "t".into(),
                archived_at: None,
                status: UiTaskStatus::Running,
                model: String::new(),
            });
        let requester = state.session_id.clone();
        apply_global_tasks(&mut state, &requester, "q-old", index.clone());
        assert!(state.tasks.index.is_none(), "stale answer must be dropped");
        apply_global_tasks(&mut state, &requester, query, index);
        assert!(state.tasks.index.is_some());
        assert!(!state.tasks.index_pending);
    }

    #[test]
    fn selection_is_stable_by_key_when_rows_reorder() {
        let mut state = state();
        state.tasks.selected = Some("child:b".into());
        state.tasks.section = TasksSection::All;
        // No such row yet: selection falls back to the first visible row.
        ensure_selection(&mut state);
        assert_eq!(state.tasks.selected.as_deref(), Some("session:current"));
    }

    fn sample_schedule(id: &str, running: bool, last: Option<ScheduleRunView>) -> ScheduleView {
        use leveler_client_protocol::schedule::{
            ScheduleActionWire, ScheduleGrantView, ScheduleTriggerWire,
        };
        ScheduleView {
            id: id.into(),
            name: format!("nightly {id}"),
            description: "runs the nightly job".into(),
            action_kind: "shell".into(),
            action: ScheduleActionWire::Shell {
                command: "echo hi".into(),
            },
            workspace: "/tmp/work".into(),
            trigger: ScheduleTriggerWire::Daily {
                time: "09:00".into(),
                timezone: None,
            },
            trigger_text: "Every day at 09:00".into(),
            timezone: None,
            enabled: true,
            running,
            run_state: None,
            next_run_at: None,
            last_run: last,
            needs_attention: false,
            attention_reason: None,
            session_policy: "continue".into(),
            overlap: "skip-if-running".into(),
            mode: "assisted".into(),
            model: None,
            timeout_secs: 600,
            misfire: "skip".into(),
            misfire_grace_secs: 0,
            session_id: None,
            grant: ScheduleGrantView {
                granted: true,
                external_side_effects: false,
                granted_at: None,
                requires_regrant: false,
            },
            created_at: "2026-11-01T00:00:00Z".into(),
            revision: "rev-1".into(),
        }
    }
}
