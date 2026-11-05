//! Model-facing adapters over the scheduled-automation management API.
//!
//! Two tools, because the risk is genuinely two facts: `schedule_inspect`
//! only reads the store, while `schedule_manage` changes definitions that will
//! run later without a person present. Folding them into one `action=`-shaped
//! tool would force one static risk level to lie about the other half.
//!
//! The store stays the only authority. These adapters translate the model's
//! structured arguments into the same `ScheduleRequest` every other client
//! sends, and render the authoritative response. They do not keep a list, do
//! not decide when a schedule fires, and do not run the scheduler: `run_now`
//! asks the existing `leveler schedule dispatch` command to make one pass,
//! exactly as an OS wake would.

use std::sync::Arc;

use async_trait::async_trait;
use leveler_client_protocol::schedule::{
    ScheduleActionWire, ScheduleCheck, ScheduleDraft, ScheduleFilter, ScheduleGrantView,
    SchedulePatch, ScheduleRequest, ScheduleResponse, ScheduleRunView, ScheduleStatusView,
    ScheduleTriggerWire, ScheduleView,
};
use leveler_execution::RiskLevel;
use leveler_project::Layout;
use leveler_schedule::ScheduleManager;
use leveler_tools::{Tool, ToolContext, ToolError, ToolOutput};
use schemars::JsonSchema;
use serde::Deserialize;
use tokio_util::sync::CancellationToken;

/// Starts the existing one-pass dispatcher for an explicit run-now.
///
/// Injected rather than constructed here so a test can observe the request
/// without spawning a process, and so the composition root remains the one
/// place that knows which executable this host runs.
pub(crate) type DispatchTrigger = Arc<dyn Fn() -> Result<(), String> + Send + Sync>;

/// The production trigger: this binary's `schedule dispatch` command.
pub(crate) fn dispatch_trigger(layout: &Layout) -> DispatchTrigger {
    let executable = std::env::current_exe()
        .map_err(|error| format!("could not resolve the leveler executable: {error}"));
    let home = layout.home().root().to_path_buf();
    let config_dir = layout.config_dir.clone();
    Arc::new(move || {
        let executable = executable.clone()?;
        start_dispatch(&executable, &home, &config_dir)
    })
}

/// One short-lived dispatch pass, started detached.
///
/// Its output is discarded because a run's observed outcome belongs to the
/// schedule store, never to an unobserved pipe.
fn start_dispatch(
    executable: &std::path::Path,
    home: &std::path::Path,
    config_dir: &std::path::Path,
) -> Result<(), String> {
    tokio::process::Command::new(executable)
        .arg("schedule")
        .arg("dispatch")
        .arg("--source")
        .arg("manual")
        .arg("--home")
        .arg(home)
        .arg("--config-dir")
        .arg(config_dir)
        .arg("--json")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .map(|_child| ())
        .map_err(|error| format!("could not start `leveler schedule dispatch`: {error}"))
}

// ── schedule_inspect ────────────────────────────────────────────────────────

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
enum InspectAction {
    /// Every definition this user owns.
    List,
    /// One definition with its grant, next slot and last run.
    Show,
    /// Run history, newest first.
    Runs,
    /// Background-scheduler picture: OS registration, counts, next slot.
    Status,
    /// Why background scheduling is or is not healthy on this machine.
    Doctor,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
enum FilterInput {
    All,
    Enabled,
    Paused,
    Running,
    NeedsAttention,
    RecentFailure,
}

impl From<FilterInput> for ScheduleFilter {
    fn from(filter: FilterInput) -> Self {
        match filter {
            FilterInput::All => Self::All,
            FilterInput::Enabled => Self::Enabled,
            FilterInput::Paused => Self::Paused,
            FilterInput::Running => Self::Running,
            FilterInput::NeedsAttention => Self::NeedsAttention,
            FilterInput::RecentFailure => Self::RecentFailure,
        }
    }
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct InspectInput {
    /// What to read.
    action: InspectAction,
    /// Definition id; required by `show`.
    #[serde(default)]
    id: Option<String>,
    /// Which definitions `list` returns (default `all`).
    #[serde(default)]
    filter: Option<FilterInput>,
    /// Restrict `runs` to one definition.
    #[serde(default)]
    definition: Option<String>,
    /// Maximum runs returned (default 50, max 500).
    #[serde(default)]
    limit: Option<u32>,
}

pub(crate) struct ScheduleInspectTool {
    manager: Arc<ScheduleManager>,
}

impl ScheduleInspectTool {
    pub(crate) fn new(manager: Arc<ScheduleManager>) -> Self {
        Self { manager }
    }
}

#[async_trait]
impl Tool for ScheduleInspectTool {
    fn name(&self) -> &str {
        "schedule_inspect"
    }

    fn description(&self) -> &str {
        "Read the user's scheduled automations: definitions, the next time each one \
         will run, the last result, run history, and whether OS background wakeup is \
         registered and healthy. Read-only. Use `schedule_manage` to create, edit, \
         pause, run or remove one. A `show` result carries the `revision` that an \
         update must send back, so read before changing anything."
    }

    fn requires_workspace(&self) -> bool {
        false
    }

    fn input_schema(&self) -> serde_json::Value {
        leveler_tools::tools::schema_of::<InspectInput>()
    }

    fn risk(&self) -> RiskLevel {
        RiskLevel::Safe
    }

    fn supports_parallel(&self) -> bool {
        true
    }

    fn replay_is_side_effect_free(&self) -> bool {
        true
    }

    fn recovery_is_skippable(&self) -> bool {
        true
    }

    async fn execute(
        &self,
        input: serde_json::Value,
        _context: ToolContext,
        cancellation: CancellationToken,
    ) -> Result<ToolOutput, ToolError> {
        let input: InspectInput = leveler_tools::tools::parse_input(self.name(), input)?;
        let request = match input.action {
            InspectAction::List => ScheduleRequest::List {
                filter: input.filter.map(Into::into),
            },
            InspectAction::Show => ScheduleRequest::Show {
                id: require_id(self.name(), "show", input.id)?,
            },
            InspectAction::Runs => ScheduleRequest::Runs {
                definition: input.definition,
                limit: input.limit,
            },
            InspectAction::Status => ScheduleRequest::Status,
            InspectAction::Doctor => ScheduleRequest::WakeupDoctor,
        };
        let response = tokio::select! {
            biased;
            _ = cancellation.cancelled() => {
                return Ok(ToolOutput::error("schedule inspection cancelled"));
            }
            response = self.manager.handle(request) => response,
        };
        Ok(render_response(response))
    }
}

// ── schedule_manage ─────────────────────────────────────────────────────────

/// The declared work, mirroring `ScheduleActionWire`.
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum WorkInput {
    /// A deterministic shell command.
    Shell { command: String },
    /// One structured native action over an installed application.
    Native {
        action: String,
        /// JSON object text; empty means `{}`.
        #[serde(default)]
        input: Option<String>,
    },
    /// One real agent turn submitted to the runtime.
    Agent { goal: String },
}

impl From<WorkInput> for ScheduleActionWire {
    fn from(work: WorkInput) -> Self {
        match work {
            WorkInput::Shell { command } => Self::Shell { command },
            WorkInput::Native { action, input } => Self::Native {
                action,
                input: input.unwrap_or_default(),
            },
            WorkInput::Agent { goal } => Self::Agent { goal },
        }
    }
}

/// A trigger rule, mirroring `ScheduleTriggerWire`.
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum TriggerInput {
    /// One RFC3339 instant.
    Once { at: String },
    /// A fixed interval in seconds, anchored at `anchor` (default now).
    Every {
        every_secs: u64,
        #[serde(default)]
        anchor: Option<String>,
    },
    /// One local wall-clock time per day.
    Daily {
        /// `HH:MM`.
        time: String,
        /// IANA zone. Omit for the host's current zone; never assume UTC.
        #[serde(default)]
        timezone: Option<String>,
    },
    /// Local wall-clock time on selected weekdays (`mon`, `mon-fri`, ...).
    Weekly {
        weekdays: String,
        /// `HH:MM`.
        time: String,
        #[serde(default)]
        timezone: Option<String>,
    },
    /// Five-field cron expression.
    Cron {
        expression: String,
        #[serde(default)]
        timezone: Option<String>,
    },
}

impl From<TriggerInput> for ScheduleTriggerWire {
    fn from(trigger: TriggerInput) -> Self {
        match trigger {
            TriggerInput::Once { at } => Self::Once { at },
            TriggerInput::Every { every_secs, anchor } => Self::Every { every_secs, anchor },
            TriggerInput::Daily { time, timezone } => Self::Daily { time, timezone },
            TriggerInput::Weekly {
                weekdays,
                time,
                timezone,
            } => Self::Weekly {
                weekdays,
                time,
                timezone,
            },
            TriggerInput::Cron {
                expression,
                timezone,
            } => Self::Cron {
                expression,
                timezone,
            },
        }
    }
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
enum ManageAction {
    Create,
    Update,
    Pause,
    Resume,
    RunNow,
    Cancel,
    Remove,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct ManageInput {
    /// What to change.
    action: ManageAction,
    /// Definition id. Required by everything except `create`.
    #[serde(default)]
    id: Option<String>,
    /// `create`: display name.
    #[serde(default)]
    name: Option<String>,
    /// `create`: the directory whose runtime owns the action.
    #[serde(default)]
    workspace: Option<String>,
    /// `create` (required) / `update` (optional): what runs.
    #[serde(default)]
    work: Option<WorkInput>,
    /// `create` (required) / `update` (optional): when it runs.
    #[serde(default)]
    trigger: Option<TriggerInput>,
    /// `continue` (default) or `fresh`.
    #[serde(default)]
    session_policy: Option<String>,
    /// `skip-if-running` (default), `queue-one` or `allow-concurrent`.
    #[serde(default)]
    overlap: Option<String>,
    /// `request_approval`, `assisted` (default) or `full_access`.
    #[serde(default)]
    mode: Option<String>,
    /// `provider/model`; required for an agent action to name its model.
    /// `update` with an empty string clears it.
    #[serde(default)]
    model: Option<String>,
    #[serde(default)]
    timeout_secs: Option<u64>,
    /// `skip` (default) or `run-once`.
    #[serde(default)]
    misfire: Option<String>,
    #[serde(default)]
    misfire_grace_secs: Option<u64>,
    /// `update`: the `revision` this client last observed. Required; a stale
    /// value is refused instead of overwriting a newer definition.
    #[serde(default)]
    expected_revision: Option<String>,
}

pub(crate) struct ScheduleManageTool {
    manager: Arc<ScheduleManager>,
    dispatch: DispatchTrigger,
}

impl ScheduleManageTool {
    pub(crate) fn new(manager: Arc<ScheduleManager>, dispatch: DispatchTrigger) -> Self {
        Self { manager, dispatch }
    }
}

#[async_trait]
impl Tool for ScheduleManageTool {
    fn name(&self) -> &str {
        "schedule_manage"
    }

    fn description(&self) -> &str {
        "Create, edit, pause/resume, run now, cancel or remove the user's scheduled \
         automations. Schedules are user-global, not workspace files. A wall-clock \
         request (\"every day at 9\") MUST become a daily or weekly trigger with an \
         IANA timezone, never an approximated 24h interval. Creating or editing a \
         schedule never authorizes unattended execution and never enables OS \
         background wakeup; those are separate user actions in the CLI or UI. Report \
         both facts from the returned state. Updating requires the revision from \
         `schedule_inspect show`."
    }

    fn requires_workspace(&self) -> bool {
        false
    }

    fn input_schema(&self) -> serde_json::Value {
        leveler_tools::tools::schema_of::<ManageInput>()
    }

    fn risk(&self) -> RiskLevel {
        RiskLevel::WorkspaceWrite
    }

    fn approval_reason(&self, input: &serde_json::Value, _context: &ToolContext) -> Option<String> {
        let input: ManageInput = serde_json::from_value(input.clone()).ok()?;
        let id = input.id.as_deref().unwrap_or("(未命名计划)");
        match input.action {
            ManageAction::Create => {
                let name = input.name.as_deref().unwrap_or("(未命名计划)");
                let trigger = input
                    .trigger
                    .as_ref()
                    .map(describe_trigger)
                    .unwrap_or_else(|| "(未指定时间)".into());
                let workspace = input.workspace.as_deref().unwrap_or("(未指定工作区)");
                let mode = input.mode.as_deref().unwrap_or("assisted");
                Some(format!(
                    "创建计划「{name}」:{trigger};工作区 {workspace};权限 {mode};未授权无人值守,\
                     创建后需要用户在界面或命令行明确授权。OS 后台唤醒不会自动启用。"
                ))
            }
            ManageAction::Update => Some(format!(
                "修改计划 {id}:修改动作、工作区、模型、权限、时区或执行频率会使现有无人值守\
                 授权失效并要求重新授权。"
            )),
            ManageAction::Remove => Some(format!("删除计划 {id} 及其未执行的槽位。")),
            // Ordinary control of a definition the user already authorized.
            ManageAction::Pause
            | ManageAction::Resume
            | ManageAction::RunNow
            | ManageAction::Cancel => None,
        }
    }

    async fn execute(
        &self,
        input: serde_json::Value,
        _context: ToolContext,
        cancellation: CancellationToken,
    ) -> Result<ToolOutput, ToolError> {
        let input: ManageInput = leveler_tools::tools::parse_input(self.name(), input)?;
        let (request, run_now) = request_from_input(self.name(), input)?;
        let response = tokio::select! {
            biased;
            _ = cancellation.cancelled() => {
                return Ok(ToolOutput::error("schedule management cancelled"));
            }
            response = self.manager.handle(request) => response,
        };
        let mut output = render_response(response);
        if run_now && output.metadata["kind"] == "changed" {
            // The slot is already queued; the one-pass dispatcher is what
            // actually claims and executes it. Report both facts separately:
            // a queued slot without a running dispatcher is not a completed
            // run.
            match (self.dispatch)() {
                Ok(()) => {
                    output
                        .content
                        .push_str("dispatcher: the one-pass dispatcher was started\n");
                    output.metadata["dispatched"] = true.into();
                }
                Err(error) => {
                    output.content.push_str(&format!(
                        "dispatcher: this process could not start the dispatcher: {error}\n"
                    ));
                    output.metadata["dispatched"] = false.into();
                }
            }
        }
        Ok(output)
    }
}

/// Translate the model's arguments into the same request every other client
/// sends. Structural mistakes stay here; domain rejections come back from the
/// manager as an authoritative error response.
fn request_from_input(
    tool: &str,
    input: ManageInput,
) -> Result<(ScheduleRequest, bool), ToolError> {
    let request = match input.action {
        ManageAction::Create => {
            let name = require_field(tool, "create", "name", input.name)?;
            let workspace = require_field(tool, "create", "workspace", input.workspace)?;
            let work = input.work.ok_or_else(|| {
                invalid(tool, "`create` requires `work` (shell, native or agent)")
            })?;
            let trigger = input
                .trigger
                .ok_or_else(|| invalid(tool, "`create` requires `trigger`"))?;
            (
                ScheduleRequest::Create {
                    draft: ScheduleDraft {
                        name,
                        workspace,
                        action: work.into(),
                        trigger: trigger.into(),
                        session_policy: input.session_policy,
                        overlap: input.overlap,
                        mode: input.mode,
                        model: input.model,
                        timeout_secs: input.timeout_secs,
                        misfire: input.misfire,
                        misfire_grace_secs: input.misfire_grace_secs,
                        // Authorization and OS wakeup are user gestures, not model
                        // tool parameters. A schedule created here always starts
                        // ungranted so the user decides before the first run.
                        grant_unattended: false,
                        allow_external_side_effects: false,
                    },
                },
                false,
            )
        }
        ManageAction::Update => {
            let id = require_id(tool, "update", input.id)?;
            let expected_revision = input
                .expected_revision
                .filter(|value| !value.trim().is_empty())
                .ok_or_else(|| {
                    invalid(
                        tool,
                        "`update` requires `expected_revision`; call schedule_inspect with \
                         action \"show\" and send the revision it returned, so a newer edit \
                         is never overwritten",
                    )
                })?;
            let patch = SchedulePatch {
                name: input.name,
                workspace: input.workspace,
                action: input.work.map(Into::into),
                trigger: input.trigger.map(Into::into),
                session_policy: input.session_policy,
                overlap: input.overlap,
                mode: input.mode,
                model: input.model,
                timeout_secs: input.timeout_secs,
                misfire: input.misfire,
                misfire_grace_secs: input.misfire_grace_secs,
            };
            if patch == SchedulePatch::default() {
                return Err(invalid(tool, "`update` carries no changes"));
            }
            (
                ScheduleRequest::Update {
                    id,
                    expected_revision: Some(expected_revision),
                    patch,
                },
                false,
            )
        }
        ManageAction::Pause => (
            ScheduleRequest::Pause {
                id: require_id(tool, "pause", input.id)?,
            },
            false,
        ),
        ManageAction::Resume => (
            ScheduleRequest::Resume {
                id: require_id(tool, "resume", input.id)?,
            },
            false,
        ),
        ManageAction::RunNow => (
            ScheduleRequest::RunNow {
                id: require_id(tool, "run_now", input.id)?,
            },
            true,
        ),
        ManageAction::Cancel => (
            ScheduleRequest::Cancel {
                id: require_id(tool, "cancel", input.id)?,
            },
            false,
        ),
        ManageAction::Remove => (
            ScheduleRequest::Remove {
                id: require_id(tool, "remove", input.id)?,
            },
            false,
        ),
    };
    Ok(request)
}

fn require_id(tool: &str, action: &str, id: Option<String>) -> Result<String, ToolError> {
    let id = require_field(tool, action, "id", id)?;
    if id.trim().is_empty() {
        return Err(invalid(
            tool,
            format!("`{action}` requires `id`; list schedules first to find it"),
        ));
    }
    Ok(id)
}

fn require_field(
    tool: &str,
    action: &str,
    name: &str,
    value: Option<String>,
) -> Result<String, ToolError> {
    value.ok_or_else(|| invalid(tool, format!("`{action}` requires `{name}`")))
}

fn invalid(tool: &str, message: impl Into<String>) -> ToolError {
    ToolError::InvalidArguments {
        tool: tool.to_string(),
        message: message.into(),
    }
}

fn describe_trigger(trigger: &TriggerInput) -> String {
    match trigger {
        TriggerInput::Once { at } => format!("一次性 {at}"),
        TriggerInput::Every { every_secs, .. } => format!("每 {every_secs} 秒"),
        TriggerInput::Daily { time, timezone } => match timezone {
            Some(timezone) => format!("每天 {time} ({timezone})"),
            None => format!("每天 {time} (本机时区)"),
        },
        TriggerInput::Weekly {
            weekdays,
            time,
            timezone,
        } => match timezone {
            Some(timezone) => format!("每周 {weekdays} {time} ({timezone})"),
            None => format!("每周 {weekdays} {time} (本机时区)"),
        },
        TriggerInput::Cron {
            expression,
            timezone,
        } => match timezone {
            Some(timezone) => format!("cron `{expression}` ({timezone})"),
            None => format!("cron `{expression}` (本机时区)"),
        },
    }
}

fn render_response(response: ScheduleResponse) -> ToolOutput {
    let metadata = serde_json::to_value(&response).unwrap_or(serde_json::Value::Null);
    match response {
        ScheduleResponse::List { schedules } => {
            let mut content = format!("schedules: {}\n", schedules.len());
            if schedules.is_empty() {
                content.push_str("(no schedules)\n");
            }
            for schedule in &schedules {
                content.push_str(&format!(
                    "- id={} name={:?} enabled={} next_run_at={} run_state={} needs_attention={} trigger={:?}\n",
                    schedule.id,
                    schedule.name,
                    schedule.enabled,
                    schedule.next_run_at.as_deref().unwrap_or("none"),
                    schedule.run_state.as_deref().unwrap_or("none"),
                    schedule.needs_attention,
                    schedule.trigger_text,
                ));
            }
            ToolOutput::ok(content).with_metadata(metadata)
        }
        ScheduleResponse::Show { schedule } => {
            ToolOutput::ok(render_schedule(&schedule)).with_metadata(metadata)
        }
        ScheduleResponse::Runs { runs } => {
            let mut content = format!("runs: {}\n", runs.len());
            for run in &runs {
                content.push_str(&render_run(run));
            }
            ToolOutput::ok(content).with_metadata(metadata)
        }
        ScheduleResponse::Changed { schedule, message } => {
            let mut content = format!("change: {message}\n");
            if let Some(schedule) = &schedule {
                content.push_str(&render_schedule(schedule));
            }
            ToolOutput::ok(content).with_metadata(metadata)
        }
        ScheduleResponse::Removed { id } => {
            ToolOutput::ok(format!("removed: {id}\n")).with_metadata(metadata)
        }
        ScheduleResponse::Status { status } => {
            ToolOutput::ok(render_status(&status)).with_metadata(metadata)
        }
        ScheduleResponse::Doctor { healthy, checks } => {
            let mut content = format!("healthy: {healthy}\n");
            for check in &checks {
                content.push_str(&render_check(check));
            }
            ToolOutput::ok(content).with_metadata(metadata)
        }
        ScheduleResponse::Error { message } => {
            ToolOutput::error(format!("schedule request failed: {message}")).with_metadata(metadata)
        }
    }
}

fn render_schedule(schedule: &ScheduleView) -> String {
    let mut content = String::new();
    content.push_str(&format!("id: {}\n", schedule.id));
    content.push_str(&format!("name: {}\n", schedule.name));
    content.push_str(&format!("description: {}\n", schedule.description));
    content.push_str(&format!("workspace: {}\n", schedule.workspace));
    content.push_str(&format!("action: {}\n", describe_action(&schedule.action)));
    content.push_str(&format!("trigger: {}\n", schedule.trigger_text));
    if let Some(timezone) = &schedule.timezone {
        content.push_str(&format!("timezone: {timezone}\n"));
    }
    content.push_str(&format!("enabled: {}\n", schedule.enabled));
    content.push_str(&format!("running: {}\n", schedule.running));
    content.push_str(&format!(
        "next_run_at: {}\n",
        schedule.next_run_at.as_deref().unwrap_or("none")
    ));
    content.push_str(&format!(
        "run_state: {}\n",
        schedule.run_state.as_deref().unwrap_or("none")
    ));
    if let Some(run) = &schedule.last_run {
        content.push_str(&format!("last_run: {}\n", render_run_inline(run)));
    }
    content.push_str(&format!("needs_attention: {}\n", schedule.needs_attention));
    if let Some(reason) = &schedule.attention_reason {
        content.push_str(&format!("attention_reason: {reason}\n"));
    }
    content.push_str(&format!(
        "session_policy: {}\noverlap: {}\nmode: {}\n",
        schedule.session_policy, schedule.overlap, schedule.mode
    ));
    if let Some(model) = &schedule.model {
        content.push_str(&format!("model: {model}\n"));
    }
    content.push_str(&format!(
        "timeout_secs: {}\nmisfire: {}\nmisfire_grace_secs: {}\n",
        schedule.timeout_secs, schedule.misfire, schedule.misfire_grace_secs
    ));
    if let Some(session) = &schedule.session_id {
        content.push_str(&format!("session_id: {session}\n"));
    }
    content.push_str(&render_grant(&schedule.grant));
    content.push('\n');
    content.push_str(&format!("created_at: {}\n", schedule.created_at));
    content.push_str(&format!("revision: {}\n", schedule.revision));
    content
}

fn describe_action(action: &ScheduleActionWire) -> String {
    match action {
        ScheduleActionWire::Shell { command } => format!("shell `{command}`"),
        ScheduleActionWire::Native { action, input } if input.is_empty() => {
            format!("native `{action}`")
        }
        ScheduleActionWire::Native { action, input } => {
            format!("native `{action}` input={input}")
        }
        ScheduleActionWire::Agent { goal } => format!("agent: {goal}"),
    }
}

fn render_grant(grant: &ScheduleGrantView) -> String {
    let mut content = format!(
        "grant: granted={} requires_regrant={} external_side_effects={}",
        grant.granted, grant.requires_regrant, grant.external_side_effects
    );
    if let Some(granted_at) = &grant.granted_at {
        content.push_str(&format!(" granted_at={granted_at}"));
    }
    content.push('\n');
    content
}

fn render_run(run: &ScheduleRunView) -> String {
    let mut content = format!("- {}\n", render_run_inline(run));
    if let Some(usage) = &run.usage {
        content.push_str(&format!(
            "  usage: requests={} input_tokens={} cached_input_tokens={} output_tokens={} \
             reasoning_tokens={} cost_usd_micros={} rows_without_cost={}\n",
            usage.requests,
            usage.input_tokens,
            usage.cached_input_tokens,
            usage.output_tokens,
            usage.reasoning_tokens,
            usage.cost_usd_micros,
            usage.rows_without_cost,
        ));
    }
    content
}

fn render_run_inline(run: &ScheduleRunView) -> String {
    format!(
        "id={} state={} scheduled_for={} started_at={} finished_at={} exit_code={} \
         session_id={} provider={} error={}",
        run.id,
        run.state,
        run.scheduled_for,
        run.started_at,
        run.finished_at.as_deref().unwrap_or("none"),
        run.exit_code
            .map(|code| code.to_string())
            .unwrap_or_else(|| "none".into()),
        run.session_id.as_deref().unwrap_or("none"),
        run.provider.as_deref().unwrap_or("none"),
        run.error.as_deref().unwrap_or("none"),
    )
}

fn render_status(status: &ScheduleStatusView) -> String {
    let wakeup = &status.wakeup;
    let mut content = format!(
        "wakeup: backend={} supported={} registered={} loaded={} label={}",
        wakeup.backend, wakeup.supported, wakeup.registered, wakeup.loaded, wakeup.label
    );
    if let Some(interval) = wakeup.interval_secs {
        content.push_str(&format!(" interval_secs={interval}"));
    }
    if let Some(code) = wakeup.last_exit_code {
        content.push_str(&format!(" last_exit_code={code}"));
    }
    if let Some(detail) = &wakeup.detail {
        content.push_str(&format!(" detail={detail}"));
    }
    content.push('\n');
    content.push_str(&format!(
        "definitions={} enabled={} paused={} running={}\n",
        status.definitions, status.enabled, status.paused, status.running
    ));
    content.push_str(&format!(
        "next_occurrence: {}\n",
        status.next_occurrence.as_deref().unwrap_or("none")
    ));
    content.push_str(&format!("overdue_pending: {}\n", status.overdue_pending));
    match &status.last_os_wake {
        Some(wake) => content.push_str(&format!(
            "last_os_wake: source={} started_at={} finished_at={} claimed={} succeeded={} \
             failed={} unknown={} skipped={} missed={} error={}\n",
            wake.source,
            wake.started_at,
            wake.finished_at.as_deref().unwrap_or("none"),
            wake.claimed,
            wake.succeeded,
            wake.failed,
            wake.unknown,
            wake.skipped,
            wake.missed,
            wake.error.as_deref().unwrap_or("none"),
        )),
        None => content.push_str("last_os_wake: none\n"),
    }
    content
}

fn render_check(check: &ScheduleCheck) -> String {
    format!("- [{}] {}: {}\n", check.severity, check.name, check.detail)
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;
    use leveler_core::EnvSnapshot;
    use leveler_execution::PermissionProfile;
    use leveler_schedule::ScheduleStore;
    use serde_json::{Value, json};
    use std::path::Path;
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct Fixture {
        manager: Arc<ScheduleManager>,
        workspace: PathBuf,
        _tmp: tempfile::TempDir,
    }

    async fn fixture() -> Fixture {
        let tmp = tempfile::tempdir().unwrap();
        let store = ScheduleStore::open(&tmp.path().join("schedules.db"))
            .await
            .unwrap();
        let workspace = tmp.path().join("workspace");
        std::fs::create_dir_all(&workspace).unwrap();
        Fixture {
            manager: Arc::new(ScheduleManager::new(
                Arc::new(store),
                tmp.path().to_path_buf(),
                None,
            )),
            workspace,
            _tmp: tmp,
        }
    }

    fn context() -> ToolContext {
        ToolContext::without_workspace_with_environment(
            PermissionProfile::FullAccess,
            Arc::new(EnvSnapshot::default()),
        )
    }

    fn noop_dispatch() -> (DispatchTrigger, Arc<AtomicUsize>) {
        let calls = Arc::new(AtomicUsize::new(0));
        let counter = calls.clone();
        let trigger: DispatchTrigger = Arc::new(move || {
            counter.fetch_add(1, Ordering::SeqCst);
            Ok(())
        });
        (trigger, calls)
    }

    fn failing_dispatch() -> DispatchTrigger {
        Arc::new(|| Err("no such executable".into()))
    }

    fn create_args(workspace: &Path) -> Value {
        json!({
            "action": "create",
            "name": "morning report",
            "workspace": workspace.display().to_string(),
            "work": {"kind": "shell", "command": "echo scheduled"},
            "trigger": {"kind": "daily", "time": "09:00", "timezone": "Asia/Shanghai"},
            "mode": "full_access",
        })
    }

    async fn run(tool: &dyn Tool, args: Value) -> Result<ToolOutput, ToolError> {
        tool.execute(args, context(), CancellationToken::new())
            .await
    }

    async fn create(tool: &ScheduleManageTool, workspace: &Path) -> ToolOutput {
        run(tool, create_args(workspace)).await.unwrap()
    }

    fn schedule_id(output: &ToolOutput) -> String {
        output.metadata["schedule"]["id"]
            .as_str()
            .expect("create returns the stored schedule")
            .to_string()
    }

    #[tokio::test]
    async fn create_returns_the_stored_schedule_with_its_next_run() {
        let fixture = fixture().await;
        let (dispatch, _calls) = noop_dispatch();
        let tool = ScheduleManageTool::new(fixture.manager.clone(), dispatch);

        let output = create(&tool, &fixture.workspace).await;

        assert!(!output.is_error, "unexpected failure: {}", output.content);
        assert!(output.content.contains("name: morning report"));
        assert!(
            output
                .content
                .contains("trigger: daily at 09:00 (Asia/Shanghai)")
        );
        assert!(output.content.contains("next_run_at: 20"));
        assert!(
            output.content.contains("grant: granted=false"),
            "{}",
            output.content
        );
        assert!(
            output.content.contains("requires_regrant=true"),
            "full access is not a durable unattended grant: {}",
            output.content
        );
        assert!(output.content.contains("needs_attention: true"));
        assert!(output.content.contains("revision: "));
        assert_eq!(output.metadata["kind"], "changed");
    }

    #[tokio::test]
    async fn an_ungranted_assisted_schedule_is_reported_as_needing_authorization() {
        let fixture = fixture().await;
        let (dispatch, _calls) = noop_dispatch();
        let tool = ScheduleManageTool::new(fixture.manager.clone(), dispatch);
        let mut args = create_args(&fixture.workspace);
        args["mode"] = json!("assisted");

        let output = run(&tool, args).await.unwrap();

        assert!(!output.is_error, "{}", output.content);
        assert!(output.content.contains("grant: granted=false"));
        assert!(output.content.contains("requires_regrant=true"));
        assert!(
            output.content.contains("needs_attention: true"),
            "{}",
            output.content
        );
    }

    #[tokio::test]
    async fn changing_the_action_drops_the_grant_and_reports_regrant() {
        let fixture = fixture().await;
        let (dispatch, _calls) = noop_dispatch();
        let tool = ScheduleManageTool::new(fixture.manager.clone(), dispatch);
        let mut args = create_args(&fixture.workspace);
        args["mode"] = json!("assisted");
        let created = run(&tool, args).await.unwrap();
        assert!(
            created.content.contains("granted=false"),
            "{}",
            created.content
        );

        // The grant is a user gesture made outside the model tool. This test
        // only needs the stored grant to exist before the tool edits the
        // action; it does not exercise how the grant was issued.
        let granted = fixture
            .manager
            .handle(ScheduleRequest::Grant {
                id: schedule_id(&created),
                allow_external_side_effects: true,
            })
            .await;
        let granted_revision = match granted {
            ScheduleResponse::Changed {
                schedule: Some(schedule),
                ..
            } => schedule.revision,
            other => panic!("unexpected grant response: {other:?}"),
        };

        let updated = run(
            &tool,
            json!({
                "action": "update",
                "id": schedule_id(&created),
                "expected_revision": granted_revision,
                "work": {"kind": "shell", "command": "echo changed"},
            }),
        )
        .await
        .unwrap();

        assert!(!updated.is_error, "{}", updated.content);
        assert!(
            updated.content.contains("granted=false"),
            "{}",
            updated.content
        );
        assert!(updated.content.contains("requires_regrant=true"));
        assert!(updated.content.contains("needs_attention: true"));
        assert!(
            updated.content.contains("echo changed"),
            "the output must show what now runs: {}",
            updated.content
        );
    }

    #[tokio::test]
    async fn update_requires_the_revision_the_client_read() {
        let fixture = fixture().await;
        let (dispatch, _calls) = noop_dispatch();
        let tool = ScheduleManageTool::new(fixture.manager.clone(), dispatch);
        let created = create(&tool, &fixture.workspace).await;

        let error = run(
            &tool,
            json!({
                "action": "update",
                "id": schedule_id(&created),
                "name": "renamed",
            }),
        )
        .await
        .expect_err("a blind overwrite must be refused before it reaches the store");

        assert!(error.to_string().contains("expected_revision"), "{error}");
    }

    #[tokio::test]
    async fn run_now_starts_the_existing_dispatcher_once() {
        let fixture = fixture().await;
        let (dispatch, calls) = noop_dispatch();
        let tool = ScheduleManageTool::new(fixture.manager.clone(), dispatch);
        let created = create(&tool, &fixture.workspace).await;

        let output = run(
            &tool,
            json!({"action": "run_now", "id": schedule_id(&created)}),
        )
        .await
        .unwrap();

        assert!(!output.is_error, "{}", output.content);
        assert!(output.content.contains("run-now slot"));
        assert!(output.content.contains("dispatcher was started"));
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert_eq!(output.metadata["dispatched"], true);
    }

    #[tokio::test]
    async fn a_dispatcher_that_cannot_start_is_reported_not_hidden() {
        let fixture = fixture().await;
        let tool = ScheduleManageTool::new(fixture.manager.clone(), failing_dispatch());
        let created = create(&tool, &fixture.workspace).await;

        let output = run(
            &tool,
            json!({"action": "run_now", "id": schedule_id(&created)}),
        )
        .await
        .unwrap();

        assert!(
            !output.is_error,
            "the queued slot is a real state change; only the pass failed"
        );
        assert!(output.content.contains("run-now slot"));
        assert!(output.content.contains("could not start the dispatcher"));
        assert_eq!(output.metadata["dispatched"], false);
    }

    #[tokio::test]
    async fn list_filters_read_the_same_projection_as_show() {
        let fixture = fixture().await;
        let (dispatch, _calls) = noop_dispatch();
        let tool = ScheduleManageTool::new(fixture.manager.clone(), dispatch);
        let first = create(&tool, &fixture.workspace).await;
        let mut second_args = create_args(&fixture.workspace);
        second_args["name"] = json!("second");
        let second = run(&tool, second_args).await.unwrap();
        run(
            &tool,
            json!({"action": "pause", "id": schedule_id(&second)}),
        )
        .await
        .unwrap();

        let inspect = ScheduleInspectTool::new(fixture.manager.clone());
        let paused = run(&inspect, json!({"action": "list", "filter": "paused"}))
            .await
            .unwrap();

        assert!(
            paused.content.contains("schedules: 1"),
            "{}",
            paused.content
        );
        assert!(paused.content.contains("second"));
        assert!(!paused.content.contains(&schedule_id(&first)));
        assert_eq!(paused.metadata.as_object().unwrap().len(), 2);
        assert_eq!(paused.metadata["schedules"].as_array().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn show_is_the_read_that_yields_the_update_revision() {
        let fixture = fixture().await;
        let (dispatch, _calls) = noop_dispatch();
        let manage = ScheduleManageTool::new(fixture.manager.clone(), dispatch);
        create(&manage, &fixture.workspace).await;
        let inspect = ScheduleInspectTool::new(fixture.manager.clone());

        let list = run(&inspect, json!({"action": "list"})).await.unwrap();
        let id = list.metadata["schedules"][0]["id"]
            .as_str()
            .unwrap()
            .to_string();
        let shown = run(&inspect, json!({"action": "show", "id": id}))
            .await
            .unwrap();

        assert!(!shown.is_error, "{}", shown.content);
        assert!(shown.content.contains("revision: "));
        assert_eq!(shown.metadata["kind"], "show");
    }

    #[tokio::test]
    async fn a_domain_rejection_is_a_visible_tool_error() {
        let fixture = fixture().await;
        let (dispatch, _calls) = noop_dispatch();
        let tool = ScheduleManageTool::new(fixture.manager.clone(), dispatch);
        let mut args = create_args(&fixture.workspace);
        args["name"] = json!("   ");

        let output = run(&tool, args).await.unwrap();

        assert!(output.is_error);
        assert!(
            output.content.contains("name is empty"),
            "{}",
            output.content
        );
    }

    #[tokio::test]
    async fn changing_future_execution_asks_for_consent_and_reading_does_not() {
        let fixture = fixture().await;
        let (dispatch, _calls) = noop_dispatch();
        let tool = ScheduleManageTool::new(fixture.manager.clone(), dispatch);

        let create_reason = tool
            .approval_reason(&create_args(&fixture.workspace), &context())
            .expect("creating a schedule changes what runs later");
        assert!(create_reason.contains("morning report"), "{create_reason}");
        assert!(create_reason.contains("09:00"), "{create_reason}");

        for action in ["pause", "resume", "run_now", "cancel"] {
            assert!(
                tool.approval_reason(&json!({"action": action, "id": "s-1"}), &context())
                    .is_none(),
                "{action} is ordinary control of an already-authorized definition"
            );
        }
        assert!(
            tool.approval_reason(&json!({"action": "remove", "id": "s-1"}), &context())
                .is_some(),
            "removing a schedule changes future execution"
        );
        assert!(
            ScheduleInspectTool::new(fixture.manager)
                .approval_reason(&json!({"action": "list"}), &context())
                .is_none()
        );
    }

    #[tokio::test]
    async fn the_model_cannot_issue_authorization_or_register_os_wakeup() {
        let fixture = fixture().await;
        let (dispatch, _calls) = noop_dispatch();
        let tool = ScheduleManageTool::new(fixture.manager.clone(), dispatch);

        for action in ["grant", "revoke_grant", "wakeup_enable", "wakeup_disable"] {
            let error = run(&tool, json!({"action": action, "id": "s-1"}))
                .await
                .expect_err("authorization and OS registration are user actions");
            assert!(
                error.to_string().contains(action),
                "the refusal must name the unsupported action: {error}"
            );
        }

        let mut args = create_args(&fixture.workspace);
        args["grant_unattended"] = json!(true);
        let error = run(&tool, args)
            .await
            .expect_err("a model cannot attach a durable grant to its own schedule");
        assert!(error.to_string().contains("grant_unattended"), "{error}");

        let created = create(&tool, &fixture.workspace).await;
        assert!(
            created.content.contains("granted=false"),
            "{}",
            created.content
        );
        assert!(created.content.contains("requires_regrant=true"));
    }

    #[tokio::test]
    async fn status_separates_a_registered_wakeup_from_a_supported_one() {
        let status = ScheduleStatusView {
            wakeup: leveler_client_protocol::schedule::ScheduleWakeupView {
                backend: "launchd".into(),
                supported: true,
                label: "ai.leveler.schedule".into(),
                registered: false,
                loaded: false,
                registration_path: None,
                last_exit_code: None,
                run_count: None,
                detail: Some("not registered".into()),
                interval_secs: Some(60),
            },
            definitions: 2,
            enabled: 1,
            paused: 1,
            running: 0,
            next_occurrence: Some("2026-11-06T01:00:00.000Z".into()),
            overdue_pending: 0,
            last_os_wake: None,
        };

        let rendered = render_status(&status);

        assert!(rendered.contains("backend=launchd"));
        assert!(rendered.contains("supported=true"));
        assert!(rendered.contains("registered=false"));
        assert!(rendered.contains("definitions=2"));
    }
}
