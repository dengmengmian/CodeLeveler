//! `/schedules` — the terminal surface for scheduled automation.
//!
//! The page is a view over the runtime's schedule management API
//! (`ClientCommand::ManageSchedule`): it never opens the store, never
//! interprets a human-readable CLI transcript and never schedules anything
//! itself. Every row and every detail field is the runtime's projection, and
//! every action is a structured request. The runtime stays the single
//! authority for definitions, occurrences, runs, grants and OS registration.
//!
//! The page has three levels inside one screen: the list, the selected
//! definition's detail, and the create/edit form. Esc walks them back one
//! level. Creating or editing here is deterministic — it builds a
//! [`ScheduleRequest`] directly, with no model call.

use std::time::{Duration, Instant};

use chrono::Local;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};

use leveler_client_protocol::schedule::{
    ScheduleActionWire, ScheduleDraft, ScheduleFilter, SchedulePatch, ScheduleRequest,
    ScheduleResponse, ScheduleRunView, ScheduleStatusView, ScheduleTriggerWire, ScheduleView,
};
use leveler_client_protocol::{ClientCommand, CommandId};

use crate::action::Effect;
use crate::i18n::SchedulesText;
use crate::overlay::{Overlay, SelectionModel, SelectionOption};
use crate::screen::Screen;
use crate::state::AppState;

/// How often the open page re-reads the runtime while nothing is in flight.
/// Run state changes asynchronously (a dispatcher claims a slot), so a static
/// page would show a stale "已计划" for a run that is already executing.
const REFRESH_INTERVAL: Duration = Duration::from_secs(2);

/// How many runs the detail page asks for.
const RUN_HISTORY_LIMIT: u32 = 50;

/// Selection keys for the page-owned confirmation overlay.
pub const CONFIRM_CANCEL: &str = "cancel";
pub const CONFIRM_OK: &str = "confirm";

// ---------------------------------------------------------------------------
// Page state
// ---------------------------------------------------------------------------

/// Everything the schedule page holds, including the query ids it owns.
#[derive(Debug)]
pub struct ScheduleState {
    pub filter: ScheduleFilter,
    pub items: Vec<ScheduleView>,
    pub selected: usize,
    /// The definition whose detail is open. It is the runtime's projection,
    /// kept as its own copy so a filtered-out row can still be read.
    pub detail: Option<ScheduleView>,
    pub runs: Vec<ScheduleRunView>,
    pub runs_loaded: bool,
    pub status: Option<ScheduleStatusView>,
    pub form: Option<ScheduleForm>,
    /// What the last management request was, so the runtime's answer can be
    /// labelled with the action the user actually took.
    pub pending_action: Option<ScheduleActionIntent>,
    /// The runtime's own message for the last accepted change.
    pub notice: Option<String>,
    /// A page-level failure (a read failed, or a change was refused).
    pub error: Option<String>,
    pub list_query: Option<CommandId>,
    pub detail_query: Option<CommandId>,
    pub runs_query: Option<CommandId>,
    pub status_query: Option<CommandId>,
    pub manage_query: Option<CommandId>,
    pub last_refresh: Option<Instant>,
    /// A change landed; re-read the authoritative state on the next reduce.
    pub refresh_pending: bool,
}

impl Default for ScheduleState {
    fn default() -> Self {
        Self {
            filter: ScheduleFilter::All,
            items: Vec::new(),
            selected: 0,
            detail: None,
            runs: Vec::new(),
            runs_loaded: false,
            status: None,
            form: None,
            pending_action: None,
            notice: None,
            error: None,
            list_query: None,
            detail_query: None,
            runs_query: None,
            status_query: None,
            manage_query: None,
            last_refresh: None,
            refresh_pending: false,
        }
    }
}

impl ScheduleState {
    fn selected_view(&self) -> Option<&ScheduleView> {
        self.items.get(self.selected)
    }

    fn focused_view(&self) -> Option<&ScheduleView> {
        self.detail.as_ref().or_else(|| self.selected_view())
    }

    fn view_by_id(&self, id: &str) -> Option<&ScheduleView> {
        self.detail
            .as_ref()
            .filter(|view| view.id == id)
            .or_else(|| self.items.iter().find(|view| view.id == id))
    }

    fn busy(&self) -> bool {
        self.list_query.is_some()
            || self.detail_query.is_some()
            || self.runs_query.is_some()
            || self.status_query.is_some()
            || self.manage_query.is_some()
    }

    fn forget_queries(&mut self) {
        self.list_query = None;
        self.detail_query = None;
        self.runs_query = None;
        self.status_query = None;
        self.manage_query = None;
    }

    fn clear(&mut self) {
        *self = Self::default();
    }
}

/// What a management request asked for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScheduleActionIntent {
    Create,
    Update,
    Pause,
    Resume,
    RunNow,
    Cancel,
    Grant,
    RevokeGrant,
    WakeupEnable,
    WakeupDisable,
    Remove,
}

impl ScheduleActionIntent {
    fn success_label(self, t: &SchedulesText) -> &'static str {
        match self {
            Self::Create => t.saved_create,
            Self::Update => t.saved_update,
            Self::Pause => t.saved_paused,
            Self::Resume => t.saved_resumed,
            Self::RunNow => t.saved_run,
            Self::Cancel => t.saved_cancel,
            Self::Grant => t.saved_granted,
            Self::RevokeGrant => t.saved_revoked,
            Self::WakeupEnable => t.saved_wakeup_on,
            Self::WakeupDisable => t.saved_wakeup_off,
            Self::Remove => t.saved_removed,
        }
    }
}

/// Which panel the page is showing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Pane {
    List,
    Detail,
    Form,
}

fn pane(state: &AppState) -> Pane {
    if state.schedule.form.is_some() {
        Pane::Form
    } else if state.schedule.detail.is_some() {
        Pane::Detail
    } else {
        Pane::List
    }
}

// ---------------------------------------------------------------------------
// Form model
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScheduleActionKind {
    Shell,
    Agent,
}

impl ScheduleActionKind {
    fn label(self, t: &SchedulesText) -> &'static str {
        match self {
            Self::Shell => t.action_shell,
            Self::Agent => t.action_agent,
        }
    }

    fn next(self, forward: bool) -> Self {
        match (self, forward) {
            (Self::Shell, true) | (Self::Agent, false) => Self::Agent,
            _ => Self::Shell,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScheduleTriggerKind {
    Once,
    Every,
    Daily,
    Weekly,
    Cron,
}

impl ScheduleTriggerKind {
    fn label(self, t: &SchedulesText) -> &'static str {
        match self {
            Self::Once => t.trigger_once,
            Self::Every => t.trigger_every,
            Self::Daily => t.trigger_daily,
            Self::Weekly => t.trigger_weekly,
            Self::Cron => t.trigger_cron,
        }
    }

    fn hint(self, t: &SchedulesText) -> &'static str {
        match self {
            Self::Once => t.trigger_once_hint,
            Self::Every => t.trigger_every_hint,
            Self::Daily => t.trigger_daily_hint,
            Self::Weekly => t.trigger_weekly_hint,
            Self::Cron => t.trigger_cron_hint,
        }
    }

    fn next(self, forward: bool) -> Self {
        let all = [
            Self::Once,
            Self::Every,
            Self::Daily,
            Self::Weekly,
            Self::Cron,
        ];
        let index = all.iter().position(|kind| *kind == self).unwrap_or(0);
        let next = if forward {
            (index + 1) % all.len()
        } else {
            (index + all.len() - 1) % all.len()
        };
        all[next]
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScheduleMode {
    RequestApproval,
    Assisted,
    FullAccess,
}

impl ScheduleMode {
    fn as_str(self) -> &'static str {
        match self {
            Self::RequestApproval => "request_approval",
            Self::Assisted => "assisted",
            Self::FullAccess => "full_access",
        }
    }

    fn label(self, t: &SchedulesText) -> &'static str {
        match self {
            Self::RequestApproval => t.mode_request_approval,
            Self::Assisted => t.mode_assisted,
            Self::FullAccess => t.mode_full_access,
        }
    }

    fn parse(raw: &str) -> Option<Self> {
        match raw {
            "request_approval" => Some(Self::RequestApproval),
            "assisted" => Some(Self::Assisted),
            "full_access" => Some(Self::FullAccess),
            _ => None,
        }
    }

    fn next(self, forward: bool) -> Self {
        let all = [Self::Assisted, Self::FullAccess, Self::RequestApproval];
        let index = all.iter().position(|mode| *mode == self).unwrap_or(0);
        let next = if forward {
            (index + 1) % all.len()
        } else {
            (index + all.len() - 1) % all.len()
        };
        all[next]
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScheduleField {
    Name,
    Workspace,
    ActionKind,
    ActionText,
    TriggerKind,
    TriggerText,
    Mode,
    Grant,
}

const CREATE_FIELDS: &[ScheduleField] = &[
    ScheduleField::Name,
    ScheduleField::Workspace,
    ScheduleField::ActionKind,
    ScheduleField::ActionText,
    ScheduleField::TriggerKind,
    ScheduleField::TriggerText,
    ScheduleField::Mode,
    ScheduleField::Grant,
];

/// Editing never rewrites the unattended grant: authorization is a separate,
/// explicit action (`g`/`G` on the detail page), so an edited definition cannot
/// silently inherit a stale authority.
const EDIT_FIELDS: &[ScheduleField] = &[
    ScheduleField::Name,
    ScheduleField::Workspace,
    ScheduleField::ActionKind,
    ScheduleField::ActionText,
    ScheduleField::TriggerKind,
    ScheduleField::TriggerText,
    ScheduleField::Mode,
];

/// The definition being edited and the revision the editor started from.
#[derive(Debug, Clone)]
pub struct ScheduleEditTarget {
    pub id: String,
    pub revision: String,
}

#[derive(Debug, Clone)]
pub struct ScheduleForm {
    pub editing: Option<ScheduleEditTarget>,
    /// The prefilled trigger of an edit, reused verbatim when the user left it
    /// untouched so its anchor (an interval's zero point) survives.
    pub original_trigger: Option<ScheduleTriggerWire>,
    pub focus: ScheduleField,
    pub cursor: usize,
    pub name: String,
    pub workspace: String,
    pub action_kind: ScheduleActionKind,
    pub action_text: String,
    pub trigger_kind: ScheduleTriggerKind,
    pub trigger_text: String,
    pub mode: ScheduleMode,
    pub grant: bool,
    pub submitting: bool,
    pub error: Option<String>,
}

impl ScheduleForm {
    fn create() -> Self {
        Self {
            editing: None,
            original_trigger: None,
            focus: ScheduleField::Name,
            cursor: 0,
            name: String::new(),
            // The client cannot know the runtime's workspace, so it does not
            // guess one: the field starts empty and the user fills it in.
            workspace: String::new(),
            action_kind: ScheduleActionKind::Shell,
            action_text: String::new(),
            trigger_kind: ScheduleTriggerKind::Daily,
            trigger_text: String::new(),
            mode: ScheduleMode::Assisted,
            grant: false,
            submitting: false,
            error: None,
        }
    }

    fn from_view(view: &ScheduleView) -> Self {
        let (action_kind, action_text) = match &view.action {
            ScheduleActionWire::Shell { command } => (ScheduleActionKind::Shell, command.clone()),
            ScheduleActionWire::Agent { goal } => (ScheduleActionKind::Agent, goal.clone()),
            // Unreachable: `begin_edit` refuses a native action before this
            // point. Keeping the arm total means the form cannot be built for
            // a native action by accident.
            ScheduleActionWire::Native { .. } => (ScheduleActionKind::Shell, String::new()),
        };
        let trigger_kind = match &view.trigger {
            ScheduleTriggerWire::Once { .. } => ScheduleTriggerKind::Once,
            ScheduleTriggerWire::Every { .. } => ScheduleTriggerKind::Every,
            ScheduleTriggerWire::Daily { .. } => ScheduleTriggerKind::Daily,
            ScheduleTriggerWire::Weekly { .. } => ScheduleTriggerKind::Weekly,
            ScheduleTriggerWire::Cron { .. } => ScheduleTriggerKind::Cron,
        };
        let trigger_text = trigger_form_text(&view.trigger);
        Self {
            editing: Some(ScheduleEditTarget {
                id: view.id.clone(),
                revision: view.revision.clone(),
            }),
            original_trigger: Some(view.trigger.clone()),
            focus: ScheduleField::Name,
            cursor: view.name.chars().count(),
            name: view.name.clone(),
            workspace: view.workspace.clone(),
            action_kind,
            action_text,
            trigger_kind,
            trigger_text,
            mode: ScheduleMode::parse(&view.mode).unwrap_or(ScheduleMode::Assisted),
            grant: false,
            submitting: false,
            error: None,
        }
    }

    fn fields(&self) -> &'static [ScheduleField] {
        if self.editing.is_some() {
            EDIT_FIELDS
        } else {
            CREATE_FIELDS
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

    fn text_mut(&mut self, field: ScheduleField) -> Option<&mut String> {
        match field {
            ScheduleField::Name => Some(&mut self.name),
            ScheduleField::Workspace => Some(&mut self.workspace),
            ScheduleField::ActionText => Some(&mut self.action_text),
            ScheduleField::TriggerText => Some(&mut self.trigger_text),
            ScheduleField::ActionKind
            | ScheduleField::TriggerKind
            | ScheduleField::Mode
            | ScheduleField::Grant => None,
        }
    }

    fn text(&self, field: ScheduleField) -> Option<&String> {
        match field {
            ScheduleField::Name => Some(&self.name),
            ScheduleField::Workspace => Some(&self.workspace),
            ScheduleField::ActionText => Some(&self.action_text),
            ScheduleField::TriggerText => Some(&self.trigger_text),
            ScheduleField::ActionKind
            | ScheduleField::TriggerKind
            | ScheduleField::Mode
            | ScheduleField::Grant => None,
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

    fn cycle(&mut self, forward: bool) {
        match self.focus {
            ScheduleField::ActionKind => self.action_kind = self.action_kind.next(forward),
            ScheduleField::TriggerKind => self.trigger_kind = self.trigger_kind.next(forward),
            ScheduleField::Mode => self.mode = self.mode.next(forward),
            ScheduleField::Grant => {}
            ScheduleField::Name
            | ScheduleField::Workspace
            | ScheduleField::ActionText
            | ScheduleField::TriggerText => self.move_cursor(forward),
        }
    }

    fn move_cursor(&mut self, forward: bool) {
        let len = self
            .text(self.focus)
            .map(|text| text.chars().count())
            .unwrap_or(0);
        self.cursor = if forward {
            (self.cursor + 1).min(len)
        } else {
            self.cursor.saturating_sub(1)
        };
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
        let cursor = self.cursor;
        if cursor == 0 {
            return;
        }
        if let Some(text) = self.text_mut(self.focus) {
            if let Some((byte, _)) = text.char_indices().nth(cursor - 1) {
                text.remove(byte);
            }
            self.cursor -= 1;
        }
    }

    fn delete(&mut self) {
        let cursor = self.cursor;
        if let Some(text) = self.text_mut(self.focus)
            && let Some((byte, _)) = text.char_indices().nth(cursor)
        {
            text.remove(byte);
        }
    }
}

// ---------------------------------------------------------------------------
// Trigger form text
// ---------------------------------------------------------------------------

/// The compact text the form shows for a trigger, and parses back.
fn trigger_form_text(trigger: &ScheduleTriggerWire) -> String {
    match trigger {
        ScheduleTriggerWire::Once { at } => at.clone(),
        ScheduleTriggerWire::Every { every_secs, .. } => every_secs.to_string(),
        ScheduleTriggerWire::Daily { time, timezone } => match timezone {
            Some(zone) => format!("{time} {zone}"),
            None => time.clone(),
        },
        ScheduleTriggerWire::Weekly {
            weekdays,
            time,
            timezone,
        } => match timezone {
            Some(zone) => format!("{weekdays} {time} {zone}"),
            None => format!("{weekdays} {time}"),
        },
        ScheduleTriggerWire::Cron {
            expression,
            timezone,
        } => match timezone {
            Some(zone) => format!("{expression} {zone}"),
            None => expression.clone(),
        },
    }
}

/// Turn the form's compact text into the wire trigger.
///
/// This is a client-side re-encoding of a field the user typed, not domain
/// validation: the manager still parses the calendar rule, the zone and the
/// cron expression and its rejection is what the page shows.
fn parse_trigger(
    kind: ScheduleTriggerKind,
    raw: &str,
    t: &SchedulesText,
) -> Result<ScheduleTriggerWire, &'static str> {
    let text = raw.trim();
    if text.is_empty() {
        return Err(t.trigger_required);
    }
    let mut parts = text.split_whitespace();
    match kind {
        ScheduleTriggerKind::Once => Ok(ScheduleTriggerWire::Once {
            at: text.to_string(),
        }),
        ScheduleTriggerKind::Every => {
            let secs = parts
                .next()
                .and_then(|value| value.parse::<u64>().ok())
                .filter(|secs| *secs > 0)
                .ok_or(t.trigger_interval_invalid)?;
            Ok(ScheduleTriggerWire::Every {
                every_secs: secs,
                anchor: None,
            })
        }
        ScheduleTriggerKind::Daily => {
            let time = parts.next().ok_or(t.trigger_time_invalid)?;
            Ok(ScheduleTriggerWire::Daily {
                time: time.to_string(),
                timezone: join_rest(parts),
            })
        }
        ScheduleTriggerKind::Weekly => {
            let weekdays = parts.next().ok_or(t.trigger_time_invalid)?;
            let time = parts.next().ok_or(t.trigger_time_invalid)?;
            Ok(ScheduleTriggerWire::Weekly {
                weekdays: weekdays.to_string(),
                time: time.to_string(),
                timezone: join_rest(parts),
            })
        }
        ScheduleTriggerKind::Cron => {
            let expression: Vec<&str> = parts.by_ref().take(5).collect();
            if expression.len() < 5 {
                return Err(t.trigger_cron_invalid);
            }
            Ok(ScheduleTriggerWire::Cron {
                expression: expression.join(" "),
                timezone: join_rest(parts),
            })
        }
    }
}

fn join_rest(mut parts: std::str::SplitWhitespace<'_>) -> Option<String> {
    let rest: Vec<&str> = parts.by_ref().collect();
    (!rest.is_empty()).then(|| rest.join(" "))
}

/// A rule in the UI's language, built from the structured trigger — never from
/// the manager's English `trigger_text`.
fn trigger_label(trigger: &ScheduleTriggerWire, t: &SchedulesText) -> String {
    let zone = |timezone: &Option<String>| match timezone {
        Some(zone) => format!(" ({zone})"),
        None => String::new(),
    };
    match trigger {
        ScheduleTriggerWire::Once { at } => {
            format!("{} · {}", t.trigger_once, fmt_time(at))
        }
        ScheduleTriggerWire::Every { every_secs, .. } => {
            if every_secs % 3600 == 0 && *every_secs >= 3600 {
                format!("{} {} {}", t.trigger_every, every_secs / 3600, t.unit_hours)
            } else if every_secs % 60 == 0 && *every_secs >= 60 {
                format!("{} {} {}", t.trigger_every, every_secs / 60, t.unit_minutes)
            } else {
                format!("{} {} {}", t.trigger_every, every_secs, t.unit_seconds)
            }
        }
        ScheduleTriggerWire::Daily { time, timezone } => {
            format!("{} {time}{}", t.trigger_daily, zone(timezone))
        }
        ScheduleTriggerWire::Weekly {
            weekdays,
            time,
            timezone,
        } => format!("{} {weekdays} {time}{}", t.trigger_weekly, zone(timezone)),
        ScheduleTriggerWire::Cron {
            expression,
            timezone,
        } => format!("Cron {expression}{}", zone(timezone)),
    }
}

// ---------------------------------------------------------------------------
// Presentation helpers
// ---------------------------------------------------------------------------

/// The user-facing state of one definition.
///
/// The precedence is the point: a paused definition is paused even if its last
/// run succeeded; a run awaiting approval reads as "needs authorization", never
/// as "running"; an unknown result is never shown as a failure the user can
/// retry.
fn schedule_state(view: &ScheduleView, t: &SchedulesText) -> &'static str {
    if view.running {
        return t.state_running;
    }
    if !view.enabled {
        return t.state_paused;
    }
    if view.grant.requires_regrant {
        return t.state_needs_grant;
    }
    match view.run_state.as_deref() {
        Some("dispatching" | "running") => t.state_running,
        Some("waiting_approval") => t.state_needs_grant,
        Some("blocked") => t.state_blocked,
        Some("unknown") => t.state_unknown,
        Some("skipped") => t.state_skipped,
        Some("missed") => t.state_missed,
        Some("failed") => t.state_failed,
        Some("cancelled") => t.state_cancelled,
        Some("submitted") => t.state_submitted,
        Some("succeeded") => t.state_done,
        _ => t.state_scheduled,
    }
}

fn escape_state(view: &ScheduleView) -> bool {
    view.needs_attention
        || view.grant.requires_regrant
        || matches!(
            view.run_state.as_deref(),
            Some("failed" | "unknown" | "blocked" | "waiting_approval")
        )
}

fn fmt_time(raw: &str) -> String {
    chrono::DateTime::parse_from_rfc3339(raw)
        .map(|value| {
            value
                .with_timezone(&Local)
                .format("%Y-%m-%d %H:%M")
                .to_string()
        })
        .unwrap_or_else(|_| raw.to_string())
}

fn fmt_optional_time(raw: Option<&String>) -> String {
    raw.map(|value| fmt_time(value))
        .unwrap_or_else(|| "—".into())
}

fn fmt_cost(micros: u64) -> String {
    if micros == 0 {
        "$0".into()
    } else if micros < 10_000 {
        format!("${:.6}", micros as f64 / 1_000_000.0)
    } else {
        format!("${:.4}", micros as f64 / 1_000_000.0)
    }
}

fn filter_label(filter: ScheduleFilter, t: &SchedulesText) -> &'static str {
    match filter {
        ScheduleFilter::All => t.filter_all,
        ScheduleFilter::Enabled => t.filter_enabled,
        ScheduleFilter::Paused => t.filter_paused,
        ScheduleFilter::Running => t.filter_running,
        ScheduleFilter::NeedsAttention => t.filter_attention,
        ScheduleFilter::RecentFailure => t.filter_failed,
    }
}

fn next_filter(filter: ScheduleFilter) -> ScheduleFilter {
    match filter {
        ScheduleFilter::All => ScheduleFilter::Enabled,
        ScheduleFilter::Enabled => ScheduleFilter::Paused,
        ScheduleFilter::Paused => ScheduleFilter::Running,
        ScheduleFilter::Running => ScheduleFilter::NeedsAttention,
        ScheduleFilter::NeedsAttention => ScheduleFilter::RecentFailure,
        ScheduleFilter::RecentFailure => ScheduleFilter::All,
    }
}

/// The background-scheduler line. Registration, support and the last wake are
/// three different facts, so the line never lets "supported" read as "enabled".
fn wakeup_summary(status: Option<&ScheduleStatusView>, t: &SchedulesText) -> String {
    let Some(status) = status else {
        return t.wakeup_unknown.to_string();
    };
    let wakeup = &status.wakeup;
    let head = if !wakeup.supported {
        t.wakeup_unsupported.to_string()
    } else if wakeup.registered {
        let mut text = t.wakeup_enabled.to_string();
        if wakeup.interval_secs == Some(60) {
            text.push_str(" · ");
            text.push_str(t.wakeup_every_minute);
        }
        text
    } else {
        t.wakeup_disabled.to_string()
    };
    format!(
        "{head} · {} {} · {} {} · {} {}",
        t.summary_definitions,
        status.definitions,
        t.summary_enabled,
        status.enabled,
        t.summary_paused,
        status.paused
    )
}

fn action_kind_label(kind: &str, action: &ScheduleActionWire, t: &SchedulesText) -> &'static str {
    match (kind, action) {
        ("shell", _) => t.action_shell,
        ("agent", _) => t.action_agent,
        _ => t.action_native,
    }
}

fn action_detail(action: &ScheduleActionWire) -> String {
    match action {
        ScheduleActionWire::Shell { command } => command.clone(),
        ScheduleActionWire::Agent { goal } => goal.clone(),
        ScheduleActionWire::Native { action, .. } => action.clone(),
    }
}

fn grant_line(view: &ScheduleView, t: &SchedulesText) -> String {
    // A definition that never had a grant is missing authorization; it is not
    // a previously valid grant that must be re-issued.
    if !view.grant.granted {
        return t.grant_missing.into();
    }
    if view.grant.requires_regrant {
        return t.grant_required.into();
    }
    match view.grant.granted_at.as_deref() {
        Some(at) if view.grant.external_side_effects => {
            format!(
                "{} · {} {}",
                t.grant_granted,
                t.grant_external,
                fmt_time(at)
            )
        }
        Some(at) => format!("{} {}", t.grant_granted, fmt_time(at)),
        None => t.grant_granted.into(),
    }
}

fn run_state_label(state: &str, t: &SchedulesText) -> &'static str {
    match state {
        "dispatching" | "running" => t.state_running,
        "succeeded" => t.state_done,
        "submitted" => t.state_submitted,
        "failed" => t.state_failed,
        "unknown" => t.state_unknown,
        "skipped" => t.state_skipped,
        "missed" => t.state_missed,
        "waiting_approval" => t.state_needs_grant,
        "blocked" => t.state_blocked,
        "cancelled" => t.state_cancelled,
        _ => t.state_unknown,
    }
}

// ---------------------------------------------------------------------------
// Requests
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Slot {
    List,
    Detail,
    Runs,
    Status,
    Manage,
}

fn send(state: &mut AppState, request: ScheduleRequest, slot: Slot) -> Effect {
    let query_id = CommandId::generate();
    match slot {
        Slot::List => state.schedule.list_query = Some(query_id.clone()),
        Slot::Detail => state.schedule.detail_query = Some(query_id.clone()),
        Slot::Runs => state.schedule.runs_query = Some(query_id.clone()),
        Slot::Status => state.schedule.status_query = Some(query_id.clone()),
        Slot::Manage => state.schedule.manage_query = Some(query_id.clone()),
    }
    Effect::Send(ClientCommand::ManageSchedule {
        requester_session_id: state.session_id.clone(),
        query_id,
        request: Box::new(request),
    })
}

fn list_effect(state: &mut AppState) -> Effect {
    let filter = state.schedule.filter;
    send(
        state,
        ScheduleRequest::List {
            filter: Some(filter),
        },
        Slot::List,
    )
}

fn status_effect(state: &mut AppState) -> Effect {
    send(state, ScheduleRequest::Status, Slot::Status)
}

fn detail_effects(state: &mut AppState, id: String) -> Vec<Effect> {
    let show = send(
        state,
        ScheduleRequest::Show { id: id.clone() },
        Slot::Detail,
    );
    let runs = send(
        state,
        ScheduleRequest::Runs {
            definition: Some(id),
            limit: Some(RUN_HISTORY_LIMIT),
        },
        Slot::Runs,
    );
    vec![show, runs]
}

/// Send one management request. The page allows one in flight at a time, so a
/// slow answer can never be mislabelled as the result of a later action.
fn manage_effect(
    state: &mut AppState,
    intent: ScheduleActionIntent,
    request: ScheduleRequest,
) -> Vec<Effect> {
    if state.schedule.manage_query.is_some() {
        return Vec::new();
    }
    if !state.runtime_connected {
        state.schedule.error = Some(state.t().schedules.error_disconnected.into());
        return Vec::new();
    }
    state.schedule.pending_action = Some(intent);
    state.schedule.error = None;
    vec![send(state, request, Slot::Manage)]
}

/// Open the page, or close it when it is already open. Entering always re-reads
/// the authoritative list and the background-scheduler status.
pub fn open(state: &mut AppState) -> Vec<Effect> {
    if state.active_screen == Screen::Schedules {
        close(state);
        return Vec::new();
    }
    state.schedule.clear();
    state.active_screen = Screen::Schedules;
    state.screen_scroll = 0;
    state.schedule.last_refresh = Some(Instant::now());
    vec![list_effect(state), status_effect(state)]
}

fn close(state: &mut AppState) {
    state.active_screen = Screen::Conversation;
    state.screen_scroll = 0;
    state.schedule.forget_queries();
}

// ---------------------------------------------------------------------------
// Actions
// ---------------------------------------------------------------------------

fn open_detail(state: &mut AppState) -> Vec<Effect> {
    let Some(view) = state.schedule.selected_view().cloned() else {
        return Vec::new();
    };
    let id = view.id.clone();
    state.schedule.detail = Some(view);
    state.schedule.runs.clear();
    state.schedule.runs_loaded = false;
    state.schedule.notice = None;
    state.schedule.error = None;
    state.screen_scroll = 0;
    detail_effects(state, id)
}

fn begin_create(state: &mut AppState) -> Vec<Effect> {
    state.schedule.form = Some(ScheduleForm::create());
    state.schedule.notice = None;
    state.schedule.error = None;
    state.screen_scroll = 0;
    Vec::new()
}

fn begin_edit(state: &mut AppState) -> Vec<Effect> {
    let t = state.t();
    let Some(view) = state.schedule.focused_view().cloned() else {
        return Vec::new();
    };
    if matches!(view.action, ScheduleActionWire::Native { .. }) {
        state.schedule.error = Some(t.schedules.notice_native_edit.into());
        return Vec::new();
    }
    state.schedule.form = Some(ScheduleForm::from_view(&view));
    state.schedule.notice = None;
    state.schedule.error = None;
    state.screen_scroll = 0;
    Vec::new()
}

fn toggle_pause(state: &mut AppState) -> Vec<Effect> {
    let Some(view) = state.schedule.focused_view() else {
        return Vec::new();
    };
    let (id, enabled) = (view.id.clone(), view.enabled);
    if enabled {
        manage_effect(
            state,
            ScheduleActionIntent::Pause,
            ScheduleRequest::Pause { id },
        )
    } else {
        manage_effect(
            state,
            ScheduleActionIntent::Resume,
            ScheduleRequest::Resume { id },
        )
    }
}

fn run_now(state: &mut AppState) -> Vec<Effect> {
    let Some(view) = state.schedule.focused_view() else {
        return Vec::new();
    };
    let id = view.id.clone();
    manage_effect(
        state,
        ScheduleActionIntent::RunNow,
        ScheduleRequest::RunNow { id },
    )
}

fn cancel_running(state: &mut AppState) -> Vec<Effect> {
    let Some(view) = state.schedule.focused_view() else {
        return Vec::new();
    };
    let (id, running) = (view.id.clone(), view.running);
    if !running {
        state.schedule.error = Some(state.t().schedules.notice_not_running.into());
        return Vec::new();
    }
    manage_effect(
        state,
        ScheduleActionIntent::Cancel,
        ScheduleRequest::Cancel { id },
    )
}

fn revoke_grant(state: &mut AppState) -> Vec<Effect> {
    let Some(view) = state.schedule.focused_view() else {
        return Vec::new();
    };
    let id = view.id.clone();
    manage_effect(
        state,
        ScheduleActionIntent::RevokeGrant,
        ScheduleRequest::RevokeGrant { id },
    )
}

fn refresh_filter(state: &mut AppState) -> Vec<Effect> {
    state.schedule.filter = next_filter(state.schedule.filter);
    state.schedule.selected = 0;
    state.schedule.detail = None;
    vec![list_effect(state)]
}

// ---------------------------------------------------------------------------
// Confirmation overlay
// ---------------------------------------------------------------------------

/// Why the confirmation is open. The overlay carries the target so the answer
/// cannot be applied to a different row after the list moved.
#[derive(Debug, Clone)]
pub enum ScheduleConfirmPurpose {
    Delete { id: String },
    Grant { id: String },
    Wakeup { enable: bool },
}

/// A page-owned confirmation, reusing the shared single-select model: Esc or
/// the cancel row decides nothing, and the dangerous row is never pre-focused.
#[derive(Debug, Clone)]
pub struct ScheduleConfirm {
    pub purpose: ScheduleConfirmPurpose,
    pub model: SelectionModel,
}

fn open_confirm(state: &mut AppState, purpose: ScheduleConfirmPurpose) {
    let t = state.t();
    let s = &t.schedules;
    let (title, detail, confirm) = match &purpose {
        ScheduleConfirmPurpose::Delete { .. } => (
            s.confirm_delete_title,
            s.confirm_delete_detail,
            s.confirm_delete,
        ),
        ScheduleConfirmPurpose::Grant { .. } => (
            s.confirm_grant_title,
            s.confirm_grant_detail,
            s.confirm_grant,
        ),
        ScheduleConfirmPurpose::Wakeup { enable: true } => (
            s.confirm_wakeup_title,
            s.confirm_wakeup_detail,
            s.confirm_wakeup_on,
        ),
        ScheduleConfirmPurpose::Wakeup { enable: false } => (
            s.confirm_wakeup_off_title,
            s.confirm_wakeup_off_detail,
            s.confirm_wakeup_off,
        ),
    };
    let model = SelectionModel::new(
        title,
        vec![
            SelectionOption::new(CONFIRM_CANCEL, s.confirm_cancel),
            SelectionOption::new(CONFIRM_OK, confirm).description(detail),
        ],
        false,
    )
    .without_quick_select()
    .focus_key(CONFIRM_CANCEL);
    state.overlay = Some(Overlay::ConfirmSchedule(Box::new(ScheduleConfirm {
        purpose,
        model,
    })));
}

fn confirm_delete(state: &mut AppState) -> Vec<Effect> {
    let Some(view) = state.schedule.focused_view() else {
        return Vec::new();
    };
    let id = view.id.clone();
    open_confirm(state, ScheduleConfirmPurpose::Delete { id });
    Vec::new()
}

fn confirm_grant(state: &mut AppState) -> Vec<Effect> {
    let Some(view) = state.schedule.focused_view() else {
        return Vec::new();
    };
    let id = view.id.clone();
    open_confirm(state, ScheduleConfirmPurpose::Grant { id });
    Vec::new()
}

fn toggle_wakeup(state: &mut AppState) -> Vec<Effect> {
    let t = state.t();
    let Some(status) = state.schedule.status.as_ref() else {
        state.schedule.error = Some(t.schedules.error_status_unknown.into());
        return Vec::new();
    };
    if !status.wakeup.supported {
        state.schedule.error = Some(t.schedules.notice_wakeup_unsupported.into());
        return Vec::new();
    }
    let enable = !status.wakeup.registered;
    open_confirm(state, ScheduleConfirmPurpose::Wakeup { enable });
    Vec::new()
}

/// Answer the page-owned confirmation overlay. Called by the overlay reducer
/// only with the key the shared selection model returned on confirm.
pub(crate) fn confirmed(state: &mut AppState, purpose: ScheduleConfirmPurpose) -> Vec<Effect> {
    match purpose {
        ScheduleConfirmPurpose::Delete { id } => manage_effect(
            state,
            ScheduleActionIntent::Remove,
            ScheduleRequest::Remove { id },
        ),
        // A shell command or an agent's tools can reach outside the
        // workspace, so the terminal's explicit confirmation must include that
        // scope or the store refuses the grant. A native action is classified
        // at dispatch; its grant stays workspace-scoped here.
        ScheduleConfirmPurpose::Grant { id } => {
            let allow_external_side_effects = state
                .schedule
                .view_by_id(&id)
                .is_some_and(|view| matches!(view.action_kind.as_str(), "shell" | "agent"));
            manage_effect(
                state,
                ScheduleActionIntent::Grant,
                ScheduleRequest::Grant {
                    id,
                    allow_external_side_effects,
                },
            )
        }
        ScheduleConfirmPurpose::Wakeup { enable: true } => manage_effect(
            state,
            ScheduleActionIntent::WakeupEnable,
            ScheduleRequest::WakeupEnable,
        ),
        ScheduleConfirmPurpose::Wakeup { enable: false } => manage_effect(
            state,
            ScheduleActionIntent::WakeupDisable,
            ScheduleRequest::WakeupDisable,
        ),
    }
}

// ---------------------------------------------------------------------------
// Keys
// ---------------------------------------------------------------------------

pub fn handle_key(state: &mut AppState, key: &KeyEvent) -> Vec<Effect> {
    match pane(state) {
        Pane::Form => handle_form_key(state, key),
        Pane::Detail => handle_detail_key(state, key),
        Pane::List => handle_list_key(state, key),
    }
}

/// One level back: form → detail → list → conversation. Esc and the header's
/// `←` both call this, so the two inputs cannot diverge.
pub fn back(state: &mut AppState) {
    match pane(state) {
        Pane::Form => {
            state.schedule.form = None;
            state.schedule.error = None;
            state.screen_scroll = 0;
        }
        Pane::Detail => {
            state.schedule.detail = None;
            state.schedule.runs.clear();
            state.schedule.runs_loaded = false;
            state.schedule.notice = None;
            state.schedule.error = None;
            state.screen_scroll = 0;
        }
        Pane::List => close(state),
    }
}

fn move_selection(state: &mut AppState, delta: isize) {
    let len = state.schedule.items.len();
    if len == 0 {
        state.schedule.selected = 0;
        return;
    }
    let index = state.schedule.selected as isize + delta;
    state.schedule.selected = index.clamp(0, len as isize - 1) as usize;
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
        KeyCode::Enter | KeyCode::Char('h') => open_detail(state),
        KeyCode::Char('n') => begin_create(state),
        KeyCode::Char('e') => begin_edit(state),
        KeyCode::Char('p') => toggle_pause(state),
        KeyCode::Char('r') => run_now(state),
        KeyCode::Char('x') => cancel_running(state),
        KeyCode::Char('d') => confirm_delete(state),
        KeyCode::Char('g') => confirm_grant(state),
        KeyCode::Char('G') => revoke_grant(state),
        KeyCode::Char('w') => toggle_wakeup(state),
        KeyCode::Char('f') | KeyCode::Tab => refresh_filter(state),
        _ => Vec::new(),
    }
}

fn scroll_detail(state: &mut AppState, delta: isize) {
    let next = state.screen_scroll as isize + delta;
    state.screen_scroll = next.max(0) as usize;
}

fn handle_detail_key(state: &mut AppState, key: &KeyEvent) -> Vec<Effect> {
    match key.code {
        KeyCode::Esc => {
            back(state);
            Vec::new()
        }
        KeyCode::Up | KeyCode::Char('k') => {
            scroll_detail(state, -1);
            Vec::new()
        }
        KeyCode::Down | KeyCode::Char('j') => {
            scroll_detail(state, 1);
            Vec::new()
        }
        KeyCode::PageUp => {
            scroll_detail(state, -10);
            Vec::new()
        }
        KeyCode::PageDown => {
            scroll_detail(state, 10);
            Vec::new()
        }
        KeyCode::Char('e') => begin_edit(state),
        KeyCode::Char('p') => toggle_pause(state),
        KeyCode::Char('r') => run_now(state),
        KeyCode::Char('x') => cancel_running(state),
        KeyCode::Char('d') => confirm_delete(state),
        KeyCode::Char('g') => confirm_grant(state),
        KeyCode::Char('G') => revoke_grant(state),
        KeyCode::Char('w') => toggle_wakeup(state),
        _ => Vec::new(),
    }
}

fn handle_form_key(state: &mut AppState, key: &KeyEvent) -> Vec<Effect> {
    let Some(form) = state.schedule.form.as_mut() else {
        return Vec::new();
    };
    if form.submitting {
        // The request is in flight; the answer owns the next state change.
        return Vec::new();
    }
    match key.code {
        KeyCode::Esc => {
            back(state);
            Vec::new()
        }
        KeyCode::Up => {
            form.move_focus(false);
            Vec::new()
        }
        KeyCode::Down | KeyCode::Tab | KeyCode::Enter if !form.is_last() => {
            form.move_focus(true);
            Vec::new()
        }
        // Shift+Tab is the global permission cycle and never reaches a page.
        KeyCode::Left => {
            form.cycle(false);
            Vec::new()
        }
        KeyCode::Right => {
            form.cycle(true);
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
        KeyCode::Enter => submit_form(state),
        KeyCode::Char(' ') if form.focus == ScheduleField::Grant => {
            form.grant = !form.grant;
            Vec::new()
        }
        KeyCode::Char(ch)
            if !key
                .modifiers
                .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
        {
            form.insert_char(ch);
            Vec::new()
        }
        _ => Vec::new(),
    }
}

fn submit_form(state: &mut AppState) -> Vec<Effect> {
    let t = state.t();
    let Some(form) = state.schedule.form.as_ref() else {
        return Vec::new();
    };
    let name = form.name.trim().to_string();
    let workspace = form.workspace.trim().to_string();
    let action_text = form.action_text.trim().to_string();
    if name.is_empty() {
        return set_form_error(state, t.schedules.error_required_name);
    }
    if workspace.is_empty() {
        return set_form_error(state, t.schedules.error_required_workspace);
    }
    if action_text.is_empty() {
        return set_form_error(state, t.schedules.error_required_action);
    }
    let action = match form.action_kind {
        ScheduleActionKind::Shell => ScheduleActionWire::Shell {
            command: action_text,
        },
        ScheduleActionKind::Agent => ScheduleActionWire::Agent { goal: action_text },
    };
    // An untouched interval trigger carries an anchor the compact text cannot
    // express; reusing the original projection keeps the stored zero point.
    let trigger = match (&form.original_trigger, form.trigger_kind) {
        (Some(original), _) if trigger_form_text(original) == form.trigger_text => original.clone(),
        _ => match parse_trigger(form.trigger_kind, &form.trigger_text, &t.schedules) {
            Ok(trigger) => trigger,
            Err(message) => return set_form_error(state, message),
        },
    };
    match form.editing.clone() {
        None => {
            let draft = ScheduleDraft {
                name,
                workspace,
                action,
                trigger,
                session_policy: None,
                overlap: None,
                mode: Some(form.mode.as_str().to_string()),
                model: None,
                timeout_secs: None,
                misfire: None,
                misfire_grace_secs: None,
                grant_unattended: form.grant,
                // The create form only builds shell and agent actions, both of
                // which can reach outside the workspace.
                allow_external_side_effects: form.grant,
            };
            if let Some(form) = state.schedule.form.as_mut() {
                form.submitting = true;
                form.error = None;
            }
            manage_effect(
                state,
                ScheduleActionIntent::Create,
                ScheduleRequest::Create { draft },
            )
        }
        Some(target) => {
            let patch = SchedulePatch {
                name: Some(name),
                workspace: Some(workspace),
                action: Some(action),
                trigger: Some(trigger),
                session_policy: None,
                overlap: None,
                mode: Some(form.mode.as_str().to_string()),
                model: None,
                timeout_secs: None,
                misfire: None,
                misfire_grace_secs: None,
            };
            if let Some(form) = state.schedule.form.as_mut() {
                form.submitting = true;
                form.error = None;
            }
            manage_effect(
                state,
                ScheduleActionIntent::Update,
                ScheduleRequest::Update {
                    id: target.id,
                    expected_revision: Some(target.revision),
                    patch,
                },
            )
        }
    }
}

fn set_form_error(state: &mut AppState, message: &str) -> Vec<Effect> {
    if let Some(form) = state.schedule.form.as_mut() {
        form.error = Some(message.to_string());
        form.submitting = false;
    }
    Vec::new()
}

// ---------------------------------------------------------------------------
// Runtime answers
// ---------------------------------------------------------------------------

pub fn apply_response(state: &mut AppState, query_id: &CommandId, response: ScheduleResponse) {
    let slot = {
        let s = &mut state.schedule;
        if s.list_query.as_ref() == Some(query_id) {
            s.list_query = None;
            Some(Slot::List)
        } else if s.detail_query.as_ref() == Some(query_id) {
            s.detail_query = None;
            Some(Slot::Detail)
        } else if s.runs_query.as_ref() == Some(query_id) {
            s.runs_query = None;
            Some(Slot::Runs)
        } else if s.status_query.as_ref() == Some(query_id) {
            s.status_query = None;
            Some(Slot::Status)
        } else if s.manage_query.as_ref() == Some(query_id) {
            s.manage_query = None;
            Some(Slot::Manage)
        } else {
            // Another client's answer on the shared stream. It is not ours to
            // apply, and it must not clear a slot this page owns.
            None
        }
    };
    let Some(slot) = slot else {
        return;
    };
    match response {
        ScheduleResponse::Error { message } => {
            if slot == Slot::Manage {
                state.schedule.pending_action = None;
                if let Some(form) = state.schedule.form.as_mut() {
                    form.submitting = false;
                    form.error = Some(message);
                } else {
                    state.schedule.error = Some(message);
                }
            } else {
                state.schedule.error = Some(message);
                if slot == Slot::List {
                    // A failed read leaves the last authoritative rows on
                    // screen, and the error line says they are stale.
                    state.schedule.refresh_pending = false;
                }
            }
        }
        ScheduleResponse::List { schedules } => {
            state.schedule.items = schedules;
            state.schedule.error = None;
            if state.schedule.selected >= state.schedule.items.len() {
                state.schedule.selected = state.schedule.items.len().saturating_sub(1);
            }
            state.schedule.last_refresh = Some(Instant::now());
        }
        ScheduleResponse::Show { schedule } => {
            state.schedule.detail = Some(schedule.clone());
            replace_item(state, &schedule);
        }
        ScheduleResponse::Runs { runs } => {
            state.schedule.runs = runs;
            state.schedule.runs_loaded = true;
        }
        ScheduleResponse::Status { status } => {
            state.schedule.status = Some(status);
        }
        ScheduleResponse::Changed { schedule, message } => {
            let created = matches!(
                state.schedule.pending_action,
                Some(ScheduleActionIntent::Create)
            );
            let intent = state.schedule.pending_action.take();
            let t = state.t();
            let label = intent
                .map(|intent| intent.success_label(&t.schedules))
                .unwrap_or(t.schedules.saved_update);
            state.schedule.notice = Some(format!("{label} · {}", message));
            state.schedule.error = None;
            if let Some(schedule) = schedule {
                replace_item(state, &schedule);
                if created {
                    // A create returns the definition the runtime persisted:
                    // show its detail (including the computed next run) rather
                    // than leaving the user on a list that has not re-read yet.
                    state.schedule.detail = Some(schedule);
                    state.schedule.runs.clear();
                    state.schedule.runs_loaded = false;
                    state.screen_scroll = 0;
                } else if state
                    .schedule
                    .detail
                    .as_ref()
                    .is_some_and(|detail| detail.id == schedule.id)
                {
                    state.schedule.detail = Some(schedule);
                }
            }
            if state.schedule.form.take().is_some() {
                // The definition now has a new revision; drop the stale editor.
                // A create had no detail open; an edit returns to whatever the
                // user was reading.
                state.screen_scroll = 0;
            }
            state.schedule.refresh_pending = true;
        }
        ScheduleResponse::Removed { id } => {
            let intent = state.schedule.pending_action.take();
            let t = state.t();
            let label = intent
                .map(|intent| intent.success_label(&t.schedules))
                .unwrap_or(t.schedules.saved_removed);
            state.schedule.notice = Some(format!("{label} · {}", id));
            state.schedule.items.retain(|item| item.id != id);
            if state
                .schedule
                .detail
                .as_ref()
                .is_some_and(|detail| detail.id == id)
            {
                state.schedule.detail = None;
                state.schedule.runs.clear();
                state.schedule.runs_loaded = false;
            }
            if state.schedule.selected >= state.schedule.items.len() {
                state.schedule.selected = state.schedule.items.len().saturating_sub(1);
            }
            state.schedule.refresh_pending = true;
        }
        ScheduleResponse::Doctor { checks, .. } => {
            let runnable: Vec<String> = checks
                .iter()
                .filter(|check| check.severity != "ok")
                .map(|check| format!("{}: {}", check.name, check.detail))
                .collect();
            state.schedule.notice = Some(if runnable.is_empty() {
                state.t().schedules.doctor_ok.to_string()
            } else {
                runnable.join(" · ")
            });
        }
    }
}

fn replace_item(state: &mut AppState, schedule: &ScheduleView) {
    if let Some(item) = state
        .schedule
        .items
        .iter_mut()
        .find(|item| item.id == schedule.id)
    {
        *item = schedule.clone();
    }
}

/// Refresh the open page on a reduce pass when nothing of its own is in
/// flight. Read-only, single-flight and bounded: the list and status keep the
/// rows and the background line current, and an open detail also re-reads its
/// run history.
pub fn tail_queries(state: &mut AppState, now: Instant) -> Vec<Effect> {
    if state.active_screen != Screen::Schedules {
        return Vec::new();
    }
    if !state.runtime_connected {
        state.schedule.forget_queries();
        state.schedule.error = Some(state.t().schedules.error_disconnected.into());
        return Vec::new();
    }
    if state.schedule.busy() {
        return Vec::new();
    }
    let due = state.schedule.refresh_pending
        || state
            .schedule
            .last_refresh
            .is_none_or(|last| now.saturating_duration_since(last) >= REFRESH_INTERVAL);
    if !due {
        return Vec::new();
    }
    state.schedule.refresh_pending = false;
    state.schedule.last_refresh = Some(now);
    let mut effects = vec![list_effect(state), status_effect(state)];
    if let Some(detail) = state.schedule.detail.as_ref()
        && state.schedule.form.is_none()
    {
        let id = detail.id.clone();
        effects.extend(detail_effects(state, id));
    }
    effects
}

/// Mouse wheel over the page. The list moves its selection; the detail and
/// the form scroll their own document.
pub fn wheel(state: &mut AppState, notches: isize) {
    match pane(state) {
        Pane::List => move_selection(state, notches),
        Pane::Detail | Pane::Form => {
            let lines = (notches * 3).unsigned_abs();
            state.screen_scroll = if notches < 0 {
                state.screen_scroll.saturating_sub(lines)
            } else {
                state.screen_scroll.saturating_add(lines)
            };
        }
    }
}

/// A click on a painted list row. The key is the item index published by
/// [`render_list_rows`].
pub fn select_row(state: &mut AppState, key: &str) {
    if pane(state) != Pane::List {
        return;
    }
    if let Ok(index) = key.parse::<usize>()
        && index < state.schedule.items.len()
    {
        state.schedule.selected = index;
    }
}

// ---------------------------------------------------------------------------
// Rendering
// ---------------------------------------------------------------------------

pub fn render_screen(frame: &mut Frame, area: Rect, state: &mut AppState) {
    let t = state.t();
    let hint = match pane(state) {
        Pane::Form => t.schedules.footer_form,
        Pane::Detail => t.schedules.footer_detail,
        Pane::List => t.schedules.footer_list,
    };
    let title = match pane(state) {
        Pane::Form => {
            if state
                .schedule
                .form
                .as_ref()
                .is_some_and(|form| form.editing.is_some())
            {
                t.schedules.form_edit
            } else {
                t.schedules.form_create
            }
        }
        Pane::Detail | Pane::List => t.schedules.title,
    };
    let page = crate::secondary::SecondaryPage {
        title,
        status: crate::secondary::main_status_spans(state, area.width as usize),
        hint,
    };
    let footer_rows = hint.lines().count().max(1) as u16;
    let layout = crate::secondary::layout_with_footer(area, 0, footer_rows);
    crate::secondary::draw_header(frame, state, &layout, &page);
    crate::secondary::draw_footer(frame, &layout, &page, &state.theme);
    match pane(state) {
        Pane::Form => render_form(frame, layout.content, state),
        Pane::Detail => render_detail(frame, layout.content, state),
        Pane::List => render_list(frame, layout.content, state),
    }
}

fn push_wrapped(lines: &mut Vec<Line<'static>>, text: &str, style: Style, width: usize) {
    for row in crate::render::wrap(&crate::render::sanitize_terminal_line(text), width) {
        lines.push(Line::from(Span::styled(row, style)));
    }
}

fn summary_line(state: &AppState, width: usize) -> Line<'static> {
    let t = state.t();
    let s = &state.schedule;
    let summary = format!(
        "{}: {} · {} {} · {}",
        t.schedules.filter_label,
        filter_label(s.filter, &t.schedules),
        s.items.len(),
        t.schedules.unit_items,
        wakeup_summary(s.status.as_ref(), &t.schedules)
    );
    Line::from(Span::styled(
        crate::render::sanitize_terminal_line(&crate::render::truncate_display(&summary, width)),
        Style::default().fg(state.theme.text.muted),
    ))
}

fn render_list(frame: &mut Frame, area: Rect, state: &mut AppState) {
    let width = area.width as usize;
    if width == 0 || area.height == 0 {
        return;
    }
    let theme = state.theme.clone();
    let t = state.t();
    let mut lines = vec![summary_line(state, width)];
    if let Some(error) = state.schedule.error.clone() {
        push_wrapped(
            &mut lines,
            &error,
            Style::default().fg(theme.status.error),
            width,
        );
    }
    if let Some(notice) = state.schedule.notice.clone() {
        push_wrapped(
            &mut lines,
            &notice,
            Style::default().fg(theme.status.success),
            width,
        );
    }

    let mut rows: Vec<(usize, String)> = Vec::new();
    let mut focus = lines.len();
    if state.schedule.items.is_empty() {
        let text = if state.schedule.list_query.is_some() {
            t.schedules.loading
        } else {
            t.schedules.empty
        };
        lines.push(Line::from(Span::styled(
            text,
            Style::default().fg(theme.text.muted),
        )));
    } else {
        // Reserve the state column from the labels that actually occur, so the
        // most important cell is never the one clipped at a normal width.
        let state_w = std::iter::once(t.schedules.column_state)
            .chain(
                state
                    .schedule
                    .items
                    .iter()
                    .map(|view| schedule_state(view, &t.schedules)),
            )
            .map(unicode_width::UnicodeWidthStr::width)
            .max()
            .unwrap_or(0)
            .clamp(6, 10);
        let next_w = 16usize;
        // marker + three separators + the two fixed columns
        let fixed = 2 + 2 + 2 + 2 + next_w + state_w;
        let flexible = width.saturating_sub(fixed);
        let name_w = (flexible / 2).clamp(10, 32);
        let rule_w = flexible.saturating_sub(name_w).clamp(10, 34);
        lines.push(Line::from(Span::styled(
            format!(
                "  {}  {}  {}  {}",
                crate::render::pad_display(t.schedules.column_name, name_w),
                crate::render::pad_display(t.schedules.column_rule, rule_w),
                crate::render::pad_display(t.schedules.column_next, next_w),
                t.schedules.column_state,
            ),
            Style::default()
                .fg(theme.text.muted)
                .add_modifier(Modifier::BOLD),
        )));
        let selected = state.schedule.selected;
        for (index, view) in state.schedule.items.iter().enumerate() {
            let is_selected = index == selected;
            rows.push((lines.len(), index.to_string()));
            if is_selected {
                focus = lines.len();
            }
            let marker = if is_selected { "› " } else { "  " };
            let state_text = schedule_state(view, &t.schedules);
            let style = Style::default().fg(if escape_state(view) {
                theme.status.warning
            } else if is_selected {
                theme.accent.primary
            } else {
                theme.text.primary
            });
            let row = format!(
                "{marker}{}  {}  {}  {state_text}",
                crate::render::pad_display(
                    &crate::render::truncate_display(&view.name, name_w),
                    name_w
                ),
                crate::render::pad_display(
                    &crate::render::truncate_display(
                        &trigger_label(&view.trigger, &t.schedules),
                        rule_w
                    ),
                    rule_w
                ),
                crate::render::pad_display(
                    &crate::render::truncate_display(
                        &fmt_optional_time(view.next_run_at.as_ref()),
                        next_w
                    ),
                    next_w
                ),
            );
            lines.push(Line::from(Span::styled(
                crate::render::sanitize_terminal_line(&row),
                style,
            )));
        }
    }
    let mut hits = Vec::new();
    crate::render::render_list_rows(frame, area, lines, &rows, focus, &theme, &mut hits);
    state.screen_rows = hits;
}

fn render_detail(frame: &mut Frame, area: Rect, state: &mut AppState) {
    let width = area.width as usize;
    if width == 0 || area.height == 0 {
        return;
    }
    let theme = state.theme.clone();
    let t = state.t();
    let Some(view) = state.schedule.detail.clone() else {
        return;
    };
    let mut lines = vec![Line::from(Span::styled(
        crate::render::sanitize_terminal_line(&format!(
            "{} · {}",
            view.name,
            schedule_state(&view, &t.schedules)
        )),
        Style::default()
            .fg(theme.accent.primary)
            .add_modifier(Modifier::BOLD),
    ))];
    if let Some(notice) = state.schedule.notice.clone() {
        push_wrapped(
            &mut lines,
            &notice,
            Style::default().fg(theme.status.success),
            width,
        );
    }
    if let Some(error) = state.schedule.error.clone() {
        push_wrapped(
            &mut lines,
            &error,
            Style::default().fg(theme.status.error),
            width,
        );
    }
    let muted = Style::default().fg(theme.text.muted);
    let mut label = |name: &str, value: String| {
        lines.push(Line::from(vec![
            Span::styled(format!("{name}: "), muted),
            Span::styled(
                crate::render::sanitize_terminal_line(&value),
                Style::default().fg(theme.text.primary),
            ),
        ]));
    };
    label(t.schedules.detail_id, view.id.clone());
    label(
        t.schedules.detail_action,
        format!(
            "{} — {}",
            action_kind_label(&view.action_kind, &view.action, &t.schedules),
            action_detail(&view.action)
        ),
    );
    label(t.schedules.detail_workspace, view.workspace.clone());
    label(
        t.schedules.detail_rule,
        trigger_label(&view.trigger, &t.schedules),
    );
    label(
        t.schedules.detail_timezone,
        view.timezone.clone().unwrap_or_else(|| "—".into()),
    );
    label(
        t.schedules.detail_next,
        fmt_optional_time(view.next_run_at.as_ref()),
    );
    if let Some(run) = &view.last_run {
        label(
            t.schedules.detail_last,
            format!(
                "{} · {} · {}",
                run_state_label(&run.state, &t.schedules),
                fmt_time(&run.started_at),
                run.exit_code
                    .map(|code| format!("{} {code}", t.schedules.exit_code))
                    .unwrap_or_else(|| t.schedules.no_exit_code.into())
            ),
        );
    }
    if let Some(session) = &view.session_id {
        label(t.schedules.detail_session, session.clone());
    }
    label(
        t.schedules.detail_mode,
        ScheduleMode::parse(&view.mode)
            .map(|mode| mode.label(&t.schedules).to_string())
            .unwrap_or_else(|| view.mode.clone()),
    );
    label(
        t.schedules.detail_model,
        view.model.clone().unwrap_or_else(|| "—".into()),
    );
    label(
        t.schedules.detail_timeout,
        format!("{} {}", view.timeout_secs, t.schedules.unit_seconds),
    );
    label(t.schedules.detail_grant, grant_line(&view, &t.schedules));
    label(
        t.schedules.detail_wakeup,
        wakeup_summary(state.schedule.status.as_ref(), &t.schedules),
    );
    lines.push(Line::default());
    lines.push(Line::from(Span::styled(
        t.schedules.detail_runs,
        Style::default()
            .fg(theme.text.secondary)
            .add_modifier(Modifier::BOLD),
    )));
    if !state.schedule.runs_loaded {
        lines.push(Line::from(Span::styled(
            t.schedules.loading_runs,
            Style::default().fg(theme.text.muted),
        )));
    } else if state.schedule.runs.is_empty() {
        lines.push(Line::from(Span::styled(
            t.schedules.no_runs,
            Style::default().fg(theme.text.muted),
        )));
    } else {
        for run in &state.schedule.runs {
            let usage = run
                .usage
                .as_ref()
                .map(|usage| {
                    format!(
                        " · {} {}→{} · {}",
                        t.schedules.unit_tokens,
                        usage.input_tokens,
                        usage.output_tokens,
                        fmt_cost(usage.cost_usd_micros)
                    )
                })
                .unwrap_or_default();
            let exit = run
                .exit_code
                .map(|code| format!(" · {} {code}", t.schedules.exit_code))
                .unwrap_or_default();
            let text = format!(
                "{}  {}{}{}",
                fmt_time(&run.started_at),
                run_state_label(&run.state, &t.schedules),
                exit,
                usage
            );
            let style = if matches!(
                run.state.as_str(),
                "failed" | "unknown" | "blocked" | "waiting_approval"
            ) {
                Style::default().fg(theme.status.warning)
            } else {
                Style::default().fg(theme.text.primary)
            };
            lines.push(Line::from(Span::styled(
                crate::render::sanitize_terminal_line(&crate::render::truncate_display(
                    &text, width,
                )),
                style,
            )));
        }
    }
    crate::render::render_scrolled(frame, area, state, lines);
}

fn render_form(frame: &mut Frame, area: Rect, state: &mut AppState) {
    let width = area.width as usize;
    if width == 0 || area.height == 0 {
        return;
    }
    let theme = state.theme.clone();
    let t = state.t();
    let Some(form) = state.schedule.form.clone() else {
        return;
    };
    let muted = Style::default().fg(theme.text.muted);
    let focus_style = Style::default()
        .fg(theme.accent.primary)
        .add_modifier(Modifier::BOLD);
    let value_style = Style::default().fg(theme.text.primary);
    let mut lines: Vec<Line<'static>> = Vec::new();
    let mut focus_line = 0usize;
    for field in form.fields() {
        let focused = form.focus == *field;
        if focused && let Some(hint) = field_hint(*field, &form, &t.schedules) {
            lines.push(Line::from(Span::styled(format!("  {}", hint), muted)));
        }
        if focused {
            focus_line = lines.len();
        }
        let line = match field {
            ScheduleField::Name => text_field_line(
                focused,
                t.schedules.field_name,
                &form.name,
                form.cursor,
                focus_style,
                muted,
                value_style,
            ),
            ScheduleField::Workspace => text_field_line(
                focused,
                t.schedules.field_workspace,
                &form.workspace,
                form.cursor,
                focus_style,
                muted,
                value_style,
            ),
            ScheduleField::ActionKind => enum_field_line(
                focused,
                t.schedules.field_action,
                form.action_kind.label(&t.schedules),
                focus_style,
                muted,
                value_style,
            ),
            ScheduleField::ActionText => text_field_line(
                focused,
                match form.action_kind {
                    ScheduleActionKind::Shell => t.schedules.field_command,
                    ScheduleActionKind::Agent => t.schedules.field_goal,
                },
                &form.action_text,
                form.cursor,
                focus_style,
                muted,
                value_style,
            ),
            ScheduleField::TriggerKind => enum_field_line(
                focused,
                t.schedules.field_trigger,
                form.trigger_kind.label(&t.schedules),
                focus_style,
                muted,
                value_style,
            ),
            ScheduleField::TriggerText => text_field_line(
                focused,
                t.schedules.field_trigger_args,
                &form.trigger_text,
                form.cursor,
                focus_style,
                muted,
                value_style,
            ),
            ScheduleField::Mode => enum_field_line(
                focused,
                t.schedules.field_mode,
                form.mode.label(&t.schedules),
                focus_style,
                muted,
                value_style,
            ),
            ScheduleField::Grant => {
                let value = if form.grant {
                    t.schedules.yes
                } else {
                    t.schedules.no
                };
                enum_field_line(
                    focused,
                    t.schedules.field_grant,
                    value,
                    focus_style,
                    muted,
                    value_style,
                )
            }
        };
        lines.push(line);
    }
    lines.push(Line::default());
    if let Some(error) = &form.error {
        push_wrapped(
            &mut lines,
            error,
            Style::default().fg(theme.status.error),
            width,
        );
    }
    if form.submitting {
        lines.push(Line::from(Span::styled(
            t.schedules.saving,
            Style::default().fg(theme.text.muted),
        )));
    }
    crate::render::render_list_focused(frame, area, lines, focus_line, &theme);
}

fn text_field_line(
    focused: bool,
    label: &str,
    value: &str,
    cursor: usize,
    focus_style: Style,
    muted: Style,
    value_style: Style,
) -> Line<'static> {
    let label_style = if focused { focus_style } else { muted };
    let mut spans = vec![
        Span::styled(if focused { "› " } else { "  " }.to_string(), focus_style),
        Span::styled(format!("{label}: "), label_style),
    ];
    if focused {
        let chars: Vec<char> = value.chars().collect();
        let at = cursor.min(chars.len());
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
            crate::render::sanitize_terminal_line(value),
            value_style,
        ));
    }
    Line::from(spans)
}

fn enum_field_line(
    focused: bool,
    label: &str,
    value: &str,
    focus_style: Style,
    muted: Style,
    value_style: Style,
) -> Line<'static> {
    let label_style = if focused { focus_style } else { muted };
    Line::from(vec![
        Span::styled(if focused { "› " } else { "  " }.to_string(), focus_style),
        Span::styled(format!("{label}: "), label_style),
        Span::styled(value.to_string(), value_style),
        Span::styled(if focused { "  ←→" } else { "" }.to_string(), muted),
    ])
}

fn field_hint(field: ScheduleField, form: &ScheduleForm, t: &SchedulesText) -> Option<String> {
    match field {
        ScheduleField::Name => Some(t.hint_name.into()),
        ScheduleField::Workspace => Some(t.hint_workspace.into()),
        ScheduleField::ActionText => Some(match form.action_kind {
            ScheduleActionKind::Shell => t.hint_command.into(),
            ScheduleActionKind::Agent => t.hint_goal.into(),
        }),
        ScheduleField::TriggerText => Some(form.trigger_kind.hint(t).into()),
        ScheduleField::ActionKind | ScheduleField::TriggerKind | ScheduleField::Mode => {
            Some(t.hint_cycle.into())
        }
        ScheduleField::Grant => Some(t.hint_toggle.into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::i18n::Locale;
    use crate::overlay::SelectionOutcome;
    use crate::state::Boot;
    use crate::theme::Theme;
    use leveler_client_protocol::SessionId;
    use leveler_client_protocol::schedule::{
        ScheduleGrantView, ScheduleStatusView, ScheduleWakeupView,
    };

    fn state() -> AppState {
        AppState::new(
            Theme::no_color(),
            Boot {
                session_id: SessionId::new("tui-schedules"),
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

    fn view(id: &str) -> ScheduleView {
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
                timezone: Some("Asia/Shanghai".into()),
            },
            trigger_text: "Every day at 09:00".into(),
            timezone: Some("Asia/Shanghai".into()),
            enabled: true,
            running: false,
            run_state: None,
            next_run_at: Some("2026-11-06T01:00:00Z".into()),
            last_run: None,
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

    fn status(registered: bool, supported: bool) -> ScheduleStatusView {
        ScheduleStatusView {
            wakeup: ScheduleWakeupView {
                backend: "launchd".into(),
                supported,
                label: "com.leveler.schedule".into(),
                registered,
                loaded: registered,
                registration_path: None,
                last_exit_code: None,
                run_count: None,
                detail: None,
                interval_secs: Some(60),
            },
            definitions: 1,
            enabled: 1,
            paused: 0,
            running: 0,
            next_occurrence: None,
            overdue_pending: 0,
            last_os_wake: None,
        }
    }

    fn requests(effects: &[Effect]) -> Vec<ScheduleRequest> {
        effects
            .iter()
            .filter_map(|effect| match effect {
                Effect::Send(ClientCommand::ManageSchedule { request, .. }) => {
                    Some(request.as_ref().clone())
                }
                _ => None,
            })
            .collect()
    }

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::empty())
    }

    #[test]
    fn opening_reads_the_authoritative_state() {
        let mut state = state();
        let effects = open(&mut state);
        assert_eq!(state.active_screen, Screen::Schedules);
        let sent = requests(&effects);
        assert!(matches!(
            sent[0],
            ScheduleRequest::List {
                filter: Some(ScheduleFilter::All)
            }
        ));
        assert!(matches!(sent[1], ScheduleRequest::Status));
        assert!(state.schedule.list_query.is_some());
        assert!(state.schedule.status_query.is_some());

        // Invoking the command again closes the page and forgets its queries.
        assert!(open(&mut state).is_empty());
        assert_eq!(state.active_screen, Screen::Conversation);
        assert!(state.schedule.list_query.is_none());
    }

    #[test]
    fn answers_belonging_to_other_clients_are_ignored() {
        let mut state = state();
        let _ = open(&mut state);
        let ours = state.schedule.list_query.clone().unwrap();

        apply_response(
            &mut state,
            &CommandId::new("someone-else"),
            ScheduleResponse::List {
                schedules: vec![view("x")],
            },
        );
        assert!(state.schedule.items.is_empty());
        assert_eq!(state.schedule.list_query.as_ref(), Some(&ours));

        apply_response(
            &mut state,
            &ours,
            ScheduleResponse::List {
                schedules: vec![view("x")],
            },
        );
        assert_eq!(state.schedule.items.len(), 1);
        assert!(state.schedule.list_query.is_none());
    }

    #[test]
    fn an_accepted_change_updates_the_row_and_rereads_the_runtime() {
        let mut state = state();
        let _ = open(&mut state);
        state.schedule.items = vec![view("a")];

        let sent = requests(&toggle_pause(&mut state));
        assert!(matches!(sent[0], ScheduleRequest::Pause { .. }));
        let query = state.schedule.manage_query.clone().unwrap();

        let mut updated = view("a");
        updated.enabled = false;
        updated.revision = "rev-2".into();
        apply_response(
            &mut state,
            &query,
            ScheduleResponse::Changed {
                schedule: Some(updated),
                message: "pause recorded".into(),
            },
        );

        assert!(!state.schedule.items[0].enabled);
        assert_eq!(state.schedule.items[0].revision, "rev-2");
        let notice = state.schedule.notice.as_deref().unwrap();
        assert!(notice.starts_with(state.t().schedules.saved_paused));
        assert!(notice.contains("pause recorded"));
        assert!(state.schedule.refresh_pending);
        assert!(state.schedule.pending_action.is_none());
        assert!(state.schedule.manage_query.is_none());
    }

    #[test]
    fn a_refusal_is_never_shown_as_a_change() {
        let mut state = state();
        let _ = open(&mut state);
        state.schedule.items = vec![view("a")];
        let _ = toggle_pause(&mut state);
        let query = state.schedule.manage_query.clone().unwrap();

        apply_response(
            &mut state,
            &query,
            ScheduleResponse::Error {
                message: "revision mismatch".into(),
            },
        );

        assert_eq!(state.schedule.error.as_deref(), Some("revision mismatch"));
        assert!(state.schedule.notice.is_none());
        assert!(state.schedule.pending_action.is_none());
        assert!(state.schedule.items[0].enabled);
    }

    #[test]
    fn run_states_never_invent_a_different_meaning() {
        let state = state();
        let t = &state.t().schedules;
        let mut view = view("a");

        view.run_state = Some("waiting_approval".into());
        assert_eq!(schedule_state(&view, t), t.state_needs_grant);
        assert_ne!(schedule_state(&view, t), t.state_running);

        view.run_state = Some("unknown".into());
        assert_eq!(schedule_state(&view, t), t.state_unknown);
        assert_ne!(schedule_state(&view, t), t.state_failed);

        view.run_state = Some("submitted".into());
        assert_eq!(schedule_state(&view, t), t.state_submitted);
        assert_ne!(schedule_state(&view, t), t.state_done);

        view.run_state = Some("succeeded".into());
        assert_eq!(schedule_state(&view, t), t.state_done);

        view.running = true;
        assert_eq!(schedule_state(&view, t), t.state_running);

        view.running = false;
        view.enabled = false;
        assert_eq!(schedule_state(&view, t), t.state_paused);
    }

    #[test]
    fn escape_walks_the_form_then_the_detail_then_the_list() {
        let mut state = state();
        let _ = open(&mut state);
        state.schedule.items = vec![view("a")];
        state.schedule.detail = Some(view("a"));
        state.schedule.form = Some(ScheduleForm::create());

        back(&mut state);
        assert!(state.schedule.form.is_none());
        assert!(state.schedule.detail.is_some());
        assert_eq!(state.active_screen, Screen::Schedules);

        back(&mut state);
        assert!(state.schedule.detail.is_none());
        assert_eq!(state.active_screen, Screen::Schedules);

        back(&mut state);
        assert_eq!(state.active_screen, Screen::Conversation);
    }

    #[test]
    fn typing_edits_the_focused_field_and_required_fields_hold_the_request() {
        let mut state = state();
        let _ = open(&mut state);
        let _ = begin_create(&mut state);

        for ch in "nightly".chars() {
            assert!(handle_key(&mut state, &key(KeyCode::Char(ch))).is_empty());
        }
        assert_eq!(state.schedule.form.as_ref().unwrap().name, "nightly");

        // Workspace is still empty: nothing is sent and the form says why.
        assert!(submit_form(&mut state).is_empty());
        assert_eq!(
            state.schedule.form.as_ref().unwrap().error.as_deref(),
            Some(state.t().schedules.error_required_workspace)
        );
        assert!(state.schedule.manage_query.is_none());
    }

    #[test]
    fn create_sends_a_structured_draft_built_from_the_typed_rule() {
        let mut state = state();
        let _ = open(&mut state);
        let _ = begin_create(&mut state);
        {
            let form = state.schedule.form.as_mut().unwrap();
            form.name = "nightly".into();
            form.workspace = "/tmp/work".into();
            form.action_text = "echo hi".into();
            form.trigger_text = "09:30 Asia/Shanghai".into();
            form.grant = true;
        }

        let sent = requests(&submit_form(&mut state));
        match &sent[0] {
            ScheduleRequest::Create { draft } => {
                assert_eq!(draft.name, "nightly");
                assert_eq!(draft.workspace, "/tmp/work");
                assert_eq!(
                    draft.action,
                    ScheduleActionWire::Shell {
                        command: "echo hi".into()
                    }
                );
                assert_eq!(
                    draft.trigger,
                    ScheduleTriggerWire::Daily {
                        time: "09:30".into(),
                        timezone: Some("Asia/Shanghai".into()),
                    }
                );
                assert!(draft.grant_unattended);
                assert!(
                    draft.allow_external_side_effects,
                    "a shell grant must name the scope the action can reach"
                );
            }
            other => panic!("expected a create request, got {other:?}"),
        }
        assert!(state.schedule.form.as_ref().unwrap().submitting);
    }

    #[test]
    fn a_created_schedule_closes_the_form_and_opens_its_detail() {
        let mut state = state();
        let _ = open(&mut state);
        let _ = begin_create(&mut state);
        {
            let form = state.schedule.form.as_mut().unwrap();
            form.name = "nightly".into();
            form.workspace = "/tmp/work".into();
            form.action_text = "echo hi".into();
            form.trigger_text = "09:30".into();
        }
        let _ = submit_form(&mut state);
        let query = state.schedule.manage_query.clone().unwrap();

        let mut created = view("new");
        created.name = "nightly".into();
        apply_response(
            &mut state,
            &query,
            ScheduleResponse::Changed {
                schedule: Some(created),
                message: "schedule created".into(),
            },
        );

        assert!(state.schedule.form.is_none());
        assert_eq!(
            state.schedule.detail.as_ref().map(|view| view.id.as_str()),
            Some("new")
        );
        let notice = state.schedule.notice.as_deref().unwrap();
        assert!(notice.starts_with(state.t().schedules.saved_create));
        assert!(notice.contains("schedule created"));
        assert!(state.schedule.refresh_pending);
    }

    #[test]
    fn editing_sends_the_revision_it_read_and_keeps_an_interval_anchor() {
        let mut state = state();
        let _ = open(&mut state);
        let mut existing = view("a");
        existing.trigger = ScheduleTriggerWire::Every {
            every_secs: 3600,
            anchor: Some("2026-01-01T00:00:00Z".into()),
        };
        state.schedule.items = vec![existing.clone()];
        state.schedule.detail = Some(existing);

        assert!(begin_edit(&mut state).is_empty());
        assert!(state.schedule.form.is_some());

        let sent = requests(&submit_form(&mut state));
        match &sent[0] {
            ScheduleRequest::Update {
                id,
                expected_revision,
                patch,
            } => {
                assert_eq!(id, "a");
                assert_eq!(expected_revision.as_deref(), Some("rev-1"));
                assert_eq!(
                    patch.trigger,
                    Some(ScheduleTriggerWire::Every {
                        every_secs: 3600,
                        anchor: Some("2026-01-01T00:00:00Z".into()),
                    })
                );
            }
            other => panic!("expected an update request, got {other:?}"),
        }
    }

    #[test]
    fn a_native_action_is_not_edited_into_a_shell_command() {
        let mut state = state();
        let _ = open(&mut state);
        let mut existing = view("a");
        existing.action_kind = "native".into();
        existing.action = ScheduleActionWire::Native {
            action: "open_app".into(),
            input: "{}".into(),
        };
        state.schedule.items = vec![existing];

        assert!(begin_edit(&mut state).is_empty());
        assert!(state.schedule.form.is_none());
        assert_eq!(
            state.schedule.error.as_deref(),
            Some(state.t().schedules.notice_native_edit)
        );
    }

    #[test]
    fn delete_needs_an_explicit_confirmation_and_the_cancel_row_is_safe() {
        let mut state = state();
        let _ = open(&mut state);
        state.schedule.items = vec![view("a")];

        assert!(confirm_delete(&mut state).is_empty());
        let Some(Overlay::ConfirmSchedule(confirm)) = state.overlay.take() else {
            panic!("expected the delete confirmation");
        };
        assert!(matches!(
            confirm.purpose,
            ScheduleConfirmPurpose::Delete { .. }
        ));
        let mut model = confirm.model;
        assert_eq!(
            model.on_key(key(KeyCode::Enter)),
            SelectionOutcome::Confirm(CONFIRM_CANCEL.into())
        );
    }

    #[test]
    fn confirming_delete_sends_a_remove_for_the_captured_id() {
        let mut state = state();
        let _ = open(&mut state);
        let sent = requests(&confirmed(
            &mut state,
            ScheduleConfirmPurpose::Delete { id: "a".into() },
        ));
        assert_eq!(sent, vec![ScheduleRequest::Remove { id: "a".into() }]);
    }

    #[test]
    fn background_wakeup_is_never_enabled_silently() {
        let mut state = state();
        let _ = open(&mut state);

        assert!(toggle_wakeup(&mut state).is_empty());
        assert_eq!(
            state.schedule.error.as_deref(),
            Some(state.t().schedules.error_status_unknown)
        );

        state.schedule.status = Some(status(false, false));
        assert!(toggle_wakeup(&mut state).is_empty());
        assert_eq!(
            state.schedule.error.as_deref(),
            Some(state.t().schedules.notice_wakeup_unsupported)
        );

        state.schedule.status = Some(status(false, true));
        assert!(toggle_wakeup(&mut state).is_empty());
        let Some(Overlay::ConfirmSchedule(confirm)) = state.overlay.take() else {
            panic!("expected the wakeup confirmation");
        };
        assert!(matches!(
            confirm.purpose,
            ScheduleConfirmPurpose::Wakeup { enable: true }
        ));

        let sent = requests(&confirmed(
            &mut state,
            ScheduleConfirmPurpose::Wakeup { enable: true },
        ));
        assert_eq!(sent, vec![ScheduleRequest::WakeupEnable]);
    }

    #[test]
    fn one_management_request_is_in_flight_at_a_time() {
        let mut state = state();
        let _ = open(&mut state);
        state.schedule.items = vec![view("a")];

        assert_eq!(requests(&toggle_pause(&mut state)).len(), 1);
        assert!(toggle_pause(&mut state).is_empty());
        assert_eq!(
            state.schedule.pending_action,
            Some(ScheduleActionIntent::Pause)
        );
    }

    #[test]
    fn a_disconnected_runtime_is_not_reported_as_success() {
        let mut state = state();
        let _ = open(&mut state);
        state.schedule.items = vec![view("a")];
        state.runtime_connected = false;

        assert!(toggle_pause(&mut state).is_empty());
        assert_eq!(
            state.schedule.error.as_deref(),
            Some(state.t().schedules.error_disconnected)
        );
        assert!(state.schedule.manage_query.is_none());
        assert!(state.schedule.notice.is_none());
        assert!(state.schedule.pending_action.is_none());
    }

    #[test]
    fn rule_text_is_encoded_without_guessing_a_zone_or_interval() {
        let state = state();
        let t = &state.t().schedules;

        assert_eq!(
            parse_trigger(
                ScheduleTriggerKind::Weekly,
                "mon-fri 09:30 Asia/Shanghai",
                t
            ),
            Ok(ScheduleTriggerWire::Weekly {
                weekdays: "mon-fri".into(),
                time: "09:30".into(),
                timezone: Some("Asia/Shanghai".into()),
            })
        );
        assert_eq!(
            parse_trigger(ScheduleTriggerKind::Cron, "0 9 * * mon-fri", t),
            Ok(ScheduleTriggerWire::Cron {
                expression: "0 9 * * mon-fri".into(),
                timezone: None,
            })
        );
        // No implicit fallback to a 24-hour interval or a default zone.
        assert!(parse_trigger(ScheduleTriggerKind::Every, "0", t).is_err());
        assert!(parse_trigger(ScheduleTriggerKind::Cron, "0 9 *", t).is_err());
        assert!(parse_trigger(ScheduleTriggerKind::Daily, "   ", t).is_err());
    }

    #[test]
    fn changing_the_filter_resets_the_cursor_and_the_detail() {
        let mut state = state();
        let _ = open(&mut state);
        state.schedule.selected = 3;
        state.schedule.detail = Some(view("a"));
        let before = state.schedule.filter;

        let sent = requests(&refresh_filter(&mut state));
        assert_ne!(state.schedule.filter, before);
        assert_eq!(state.schedule.selected, 0);
        assert!(state.schedule.detail.is_none());
        assert!(matches!(sent[0], ScheduleRequest::List { .. }));
    }

    #[test]
    fn refresh_is_single_flight_and_rereads_an_open_detail_after_a_change() {
        let mut state = state();
        let _ = open(&mut state);
        let list = state.schedule.list_query.clone().unwrap();
        apply_response(
            &mut state,
            &list,
            ScheduleResponse::List {
                schedules: vec![view("a")],
            },
        );
        let status = state.schedule.status_query.clone().unwrap();
        apply_response(
            &mut state,
            &status,
            ScheduleResponse::Status {
                status: super::tests::status(false, true),
            },
        );

        let now = Instant::now();
        assert!(tail_queries(&mut state, now).is_empty(), "not due yet");

        state.schedule.refresh_pending = true;
        assert_eq!(requests(&tail_queries(&mut state, now)).len(), 2);
        assert!(
            tail_queries(&mut state, now).is_empty(),
            "only one refresh pass in flight"
        );

        let list = state.schedule.list_query.clone().unwrap();
        apply_response(
            &mut state,
            &list,
            ScheduleResponse::List {
                schedules: vec![view("a")],
            },
        );
        let status = state.schedule.status_query.clone().unwrap();
        apply_response(
            &mut state,
            &status,
            ScheduleResponse::Status {
                status: super::tests::status(false, true),
            },
        );

        state.schedule.detail = Some(view("a"));
        state.schedule.refresh_pending = true;
        assert_eq!(requests(&tail_queries(&mut state, now)).len(), 4);
    }

    #[test]
    fn the_page_does_not_refresh_while_closed() {
        let mut state = state();
        assert!(tail_queries(&mut state, Instant::now()).is_empty());
    }

    #[test]
    fn a_never_granted_definition_reports_missing_authorization() {
        let state = state();
        let text = state.t();
        let t = &text.schedules;
        let mut never = view("a");
        never.grant = ScheduleGrantView {
            granted: false,
            external_side_effects: false,
            granted_at: None,
            requires_regrant: true,
        };
        assert_eq!(grant_line(&never, t), t.grant_missing);

        let mut invalidated = view("a");
        invalidated.grant = ScheduleGrantView {
            granted: true,
            external_side_effects: false,
            granted_at: Some("2026-11-01T00:00:00Z".into()),
            requires_regrant: true,
        };
        assert_eq!(grant_line(&invalidated, t), t.grant_required);
    }

    /// Full Access is an interactive admission; it never stands in for the
    /// separate, durable unattended grant.
    #[test]
    fn full_access_mode_still_shows_the_grant_as_unset() {
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;
        use ratatui::buffer::Buffer;

        fn text(buffer: &Buffer) -> String {
            (0..buffer.area.height)
                .map(|y| {
                    (0..buffer.area.width)
                        .map(|x| buffer[(x, y)].symbol())
                        .collect::<String>()
                })
                .collect::<Vec<_>>()
                .join("\n")
        }

        let mut state = state();
        let mut form = ScheduleForm::create();
        form.mode = ScheduleMode::FullAccess;
        form.focus = ScheduleField::Grant;
        state.schedule.form = Some(form);

        let area = Rect::new(0, 0, 80, 40);
        let mut terminal = Terminal::new(TestBackend::new(area.width, area.height)).unwrap();
        terminal
            .draw(|frame| render_form(frame, area, &mut state))
            .unwrap();
        let rendered = text(terminal.backend().buffer());
        // Wide glyphs occupy two cells, so compare without layout whitespace.
        let flat: String = rendered.chars().filter(|c| !c.is_whitespace()).collect();

        assert!(flat.contains(state.t().schedules.no));
        assert!(!flat.contains("完全访问已覆盖"));
        assert!(!flat.contains("CoveredbyFullAccess"));
        let hint: String = state
            .t()
            .schedules
            .hint_toggle
            .chars()
            .filter(|c| !c.is_whitespace())
            .collect();
        assert!(flat.contains(&hint), "{rendered}");
    }
}
