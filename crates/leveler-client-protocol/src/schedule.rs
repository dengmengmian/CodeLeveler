//! Scheduled-automation management over the client protocol.
//!
//! The schedule store stays the only authority for definitions, occurrences,
//! runs and grants. This module is the structured request/response shape every
//! client (TUI, Web, Desktop) and the agent-facing management path use; it
//! never carries a human-readable command transcript that a client would have
//! to parse back into state.
//!
//! Timestamps cross the wire as RFC3339 strings so a client can render them in
//! the user's local zone without reinterpreting a numeric epoch.

use serde::{Deserialize, Serialize};

/// Which definitions a client wants to see.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum ScheduleFilter {
    All,
    Enabled,
    Paused,
    Running,
    /// Anything a person must act on: waiting approval, blocked, unknown or
    /// failed in the latest attempt.
    NeedsAttention,
    /// The latest run is `failed`, `unknown` or `blocked`.
    RecentFailure,
}

/// The declared work of a schedule, in wire form.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ScheduleActionWire {
    Shell {
        command: String,
    },
    Native {
        action: String,
        /// JSON object text; empty means `{}`.
        #[serde(default)]
        input: String,
    },
    Agent {
        goal: String,
    },
}

/// A trigger rule in the vocabulary a user can actually choose.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ScheduleTriggerWire {
    /// One RFC3339 instant.
    Once { at: String },
    /// A fixed interval in seconds, anchored at `anchor` (default now).
    Every {
        every_secs: u64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        anchor: Option<String>,
    },
    /// One local wall-clock time per day.
    Daily {
        /// `HH:MM`
        time: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        timezone: Option<String>,
    },
    /// Local wall-clock time on selected weekdays (`mon`, `mon-fri`, ...).
    Weekly {
        weekdays: String,
        /// `HH:MM`
        time: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        timezone: Option<String>,
    },
    /// Five-field cron expression.
    Cron {
        expression: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        timezone: Option<String>,
    },
}

/// The complete declaration a create request carries.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct ScheduleDraft {
    pub name: String,
    pub workspace: String,
    pub action: ScheduleActionWire,
    pub trigger: ScheduleTriggerWire,
    /// `continue` (default) or `fresh`.
    #[serde(default)]
    pub session_policy: Option<String>,
    /// `skip-if-running` (default), `queue-one` or `allow-concurrent`.
    #[serde(default)]
    pub overlap: Option<String>,
    /// `request_approval`, `assisted` (default) or `full_access`.
    #[serde(default)]
    pub mode: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_secs: Option<u64>,
    /// `skip` (default) or `run-once`.
    #[serde(default)]
    pub misfire: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub misfire_grace_secs: Option<u64>,
    /// The user explicitly asked for unattended execution.
    #[serde(default)]
    pub grant_unattended: bool,
    /// The grant also covers actions with external side effects.
    #[serde(default)]
    pub allow_external_side_effects: bool,
}

/// A partial replacement of an existing definition. Every absent field keeps
/// its stored value; an empty `model` clears the model. `expected_revision`
/// on the request is the optimistic-concurrency guard.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct SchedulePatch {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub action: Option<ScheduleActionWire>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub trigger: Option<ScheduleTriggerWire>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_policy: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub overlap: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mode: Option<String>,
    /// `Some("")` clears the model; `None` keeps it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_secs: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub misfire: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub misfire_grace_secs: Option<u64>,
}

/// One structured management request from a client.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum ScheduleRequest {
    List {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        filter: Option<ScheduleFilter>,
    },
    Show {
        id: String,
    },
    Create {
        draft: ScheduleDraft,
    },
    Update {
        id: String,
        /// The revision the client last observed. A mismatch is refused rather
        /// than overwriting a newer definition.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        expected_revision: Option<String>,
        patch: SchedulePatch,
    },
    Pause {
        id: String,
    },
    Resume {
        id: String,
    },
    RunNow {
        id: String,
    },
    Cancel {
        id: String,
    },
    Remove {
        id: String,
    },
    Runs {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        definition: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        limit: Option<u32>,
    },
    Grant {
        id: String,
        #[serde(default)]
        allow_external_side_effects: bool,
    },
    RevokeGrant {
        id: String,
    },
    /// The background-scheduler picture: OS registration, counts, next slot.
    Status,
    WakeupEnable,
    WakeupDisable,
    WakeupDoctor,
}

/// Model usage attributed to one run. Missing totals stay missing.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct ScheduleUsageView {
    pub requests: u64,
    pub input_tokens: u64,
    pub cached_input_tokens: u64,
    pub output_tokens: u64,
    pub reasoning_tokens: u64,
    pub cost_usd_micros: u64,
    pub rows_without_cost: u64,
}

/// One run, projected for a user surface.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct ScheduleRunView {
    pub id: String,
    pub state: String,
    pub scheduled_for: String,
    pub started_at: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub finished_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub execution_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub native_outcome: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub task_outcome: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub capability_state: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub permission_state: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage: Option<ScheduleUsageView>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cancel_requested_at: Option<String>,
}

/// The persisted unattended authorization, as a user surface sees it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct ScheduleGrantView {
    pub granted: bool,
    pub external_side_effects: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub granted_at: Option<String>,
    /// Unattended execution is not authorized right now: either no grant was
    /// ever issued, or a stored grant no longer matches the current
    /// definition. The user must authorize before the next scheduled run.
    pub requires_regrant: bool,
}

/// One definition with the facts a list/detail surface needs.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct ScheduleView {
    pub id: String,
    pub name: String,
    /// A one-line, user-readable statement of what runs.
    pub description: String,
    pub action_kind: String,
    pub action: ScheduleActionWire,
    pub workspace: String,
    pub trigger: ScheduleTriggerWire,
    /// The rule in words: `每天 18:00 (Asia/Shanghai)`.
    pub trigger_text: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timezone: Option<String>,
    pub enabled: bool,
    pub running: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run_state: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_run_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_run: Option<ScheduleRunView>,
    pub needs_attention: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub attention_reason: Option<String>,
    pub session_policy: String,
    pub overlap: String,
    pub mode: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    pub timeout_secs: u64,
    pub misfire: String,
    pub misfire_grace_secs: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    pub grant: ScheduleGrantView,
    pub created_at: String,
    /// Changes whenever the definition changes; a client sends it back on
    /// update so a stale editor cannot overwrite a newer definition.
    pub revision: String,
}

/// The OS wakeup registration as observed, plus what the product can do.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct ScheduleWakeupView {
    /// `launchd`, `task-scheduler` or `unsupported`.
    pub backend: String,
    pub supported: bool,
    pub label: String,
    pub registered: bool,
    pub loaded: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub registration_path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_exit_code: Option<i32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run_count: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    /// The registration's wake interval, when observed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub interval_secs: Option<u64>,
}

/// One recorded dispatch pass.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct ScheduleWakeView {
    pub id: i64,
    pub source: String,
    pub started_at: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub finished_at: Option<String>,
    pub claimed: usize,
    pub succeeded: usize,
    pub failed: usize,
    pub unknown: usize,
    pub skipped: usize,
    pub missed: usize,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// The background-scheduler status.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct ScheduleStatusView {
    pub wakeup: ScheduleWakeupView,
    pub definitions: usize,
    pub enabled: usize,
    pub paused: usize,
    pub running: usize,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_occurrence: Option<String>,
    pub overdue_pending: usize,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_os_wake: Option<ScheduleWakeView>,
}

/// One doctor check, in user language.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct ScheduleCheck {
    pub name: String,
    pub severity: String,
    pub detail: String,
}

/// The structured answer to one [`ScheduleRequest`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ScheduleResponse {
    List {
        schedules: Vec<ScheduleView>,
    },
    Show {
        schedule: ScheduleView,
    },
    Runs {
        runs: Vec<ScheduleRunView>,
    },
    Changed {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        schedule: Option<ScheduleView>,
        message: String,
    },
    Removed {
        id: String,
    },
    Status {
        status: ScheduleStatusView,
    },
    Doctor {
        healthy: bool,
        checks: Vec<ScheduleCheck>,
    },
    Error {
        message: String,
    },
}
