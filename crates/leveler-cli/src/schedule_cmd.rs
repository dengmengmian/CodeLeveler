//! `leveler schedule`: the user-global scheduled-action surface.
//!
//! Both drivers are the same dispatcher: `dispatch` is the short-lived path an
//! OS timer can invoke, and `serve` is a lightweight resident loop. Neither
//! owns a runtime, a session DB or an agent loop; both connect to the
//! workspace's ordinary runtime as a local client and disconnect after each
//! run, so the daemon's existing idle eviction stays in force.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use crate::cli::{
    RunMode, ScheduleCommand, ScheduleMisfire, ScheduleOverlap, ScheduleSessionPolicy,
    ScheduleSource,
};
use anyhow::Context as _;
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use leveler_client_protocol::{ModelRef, PermissionProfile, SessionId};
use leveler_execution::CommandRunner;
use leveler_local_transport::LocalRuntimeService;
use leveler_native::{ActionId, ActionStatus, NativeActionRequest, NativeActions, PermissionMode};
use leveler_project::Layout;
use leveler_runtime_host::{
    DetachedRuntimeLaunch, NonInteractiveHandoffUi, ensure_default_runtime,
};
use leveler_schedule::{
    DefinitionUpdate, DispatchSummary, Dispatcher, LocalTime, MisfirePolicy, NativeActionClass,
    NativeActionExecutor, NativeExecutionReport, NativeExecutionRequest, NativeRunStatus,
    NewScheduleDefinition, OverlapPolicy, RuntimeConnector, ScheduleAction, ScheduleDefinition,
    ScheduleStore, ScheduleTrigger, SessionPolicy, UnattendedRequest, UsageReader, UsageSnapshot,
    WakeSource, WeekdaySet, parse_cron,
    wakeup::{self, WAKE_INTERVAL, WakeupSpec, WakeupStatus},
};

const DISPATCH_LEASE: Duration = Duration::from_secs(120);

pub async fn cmd_schedule(
    layout: Layout,
    config_dir: Option<PathBuf>,
    command: ScheduleCommand,
) -> anyhow::Result<()> {
    let store = ScheduleStore::open(&layout.home().schedules_database())
        .await
        .context("opening the schedule store")?;
    match command {
        ScheduleCommand::Add {
            name,
            workspace,
            command,
            native_action,
            native_input,
            agent_goal,
            at,
            every,
            anchor,
            daily,
            weekly,
            local_time,
            cron,
            timezone,
            mode,
            model,
            session_policy,
            overlap,
            grant_unattended,
            allow_external_side_effects,
            timeout,
            misfire,
            misfire_grace,
            json,
        } => {
            let workspace = match workspace {
                Some(workspace) => workspace,
                None => layout
                    .primary_workspace()
                    .map(Path::to_path_buf)
                    .context("this command has no workspace; pass --workspace")?,
            };
            let action = build_action(command, native_action, native_input, agent_goal)?;
            let trigger =
                build_trigger(at, every, anchor, daily, weekly, local_time, cron, timezone)?;
            let model = match model {
                Some(reference) => Some(
                    ModelRef::parse(&reference)
                        .with_context(|| format!("`{reference}` is not a valid model reference"))?,
                ),
                None => None,
            };
            let definition = store
                .create_definition(NewScheduleDefinition {
                    name,
                    workspace,
                    action,
                    trigger,
                    session_policy: map_session_policy(session_policy),
                    overlap: map_overlap_policy(overlap),
                    mode: schedule_mode(mode),
                    model,
                    timeout: Duration::from_secs(timeout),
                    misfire: misfire_policy(misfire),
                    misfire_grace: Duration::from_secs(misfire_grace),
                    unattended: grant_unattended.then_some(UnattendedRequest {
                        allow_external_side_effects,
                    }),
                })
                .await?;
            let next = next_run_text(&store, &definition.id).await?;
            if json {
                print_json(&definition_json(&definition, next.as_deref()))?;
            } else {
                println!("added schedule {}", definition.id);
                println!("  name:       {}", definition.name);
                println!("  workspace:  {}", definition.workspace.display());
                println!("  action:     {}", action_text(&definition.action));
                println!("  trigger:    {}", trigger_text(&definition.trigger));
                println!("  session:    {}", definition.session_policy.as_str());
                println!("  overlap:    {}", definition.overlap.as_str());
                println!("  grant:      {}", grant_text(&definition));
                println!("  next:       {}", next.unwrap_or_else(|| "none".into()));
            }
        }
        ScheduleCommand::List { json } => {
            let definitions = store.definitions().await?;
            if json {
                let rows: Vec<_> = definitions
                    .iter()
                    .map(|definition| {
                        serde_json::json!({
                            "id": definition.id,
                            "name": definition.name,
                            "workspace": definition.workspace,
                            "action": action_json(&definition.action),
                            "trigger": trigger_json(&definition.trigger),
                            "session_policy": definition.session_policy.as_str(),
                            "overlap": definition.overlap.as_str(),
                            "enabled": definition.enabled,
                            "misfire": definition.misfire.as_str(),
                            "misfire_grace_secs": definition.misfire_grace.as_secs(),
                            "session_id": definition.session_id,
                            "unattended_grant": grant_json(definition),
                            "created_at": definition.created_at,
                        })
                    })
                    .collect();
                print_json(&serde_json::json!(rows))?;
            } else if definitions.is_empty() {
                println!("no schedules");
            } else {
                for definition in definitions {
                    println!(
                        "{}  {}  {}  {}  {}  session={}",
                        definition.id,
                        definition.name,
                        action_kind_text(&definition.action),
                        trigger_text(&definition.trigger),
                        if definition.enabled {
                            "enabled"
                        } else {
                            "paused"
                        },
                        definition
                            .session_id
                            .as_ref()
                            .map(|session| session.as_str())
                            .unwrap_or("-"),
                    );
                    println!(
                        "  workspace: {}  overlap: {}  grant: {}",
                        definition.workspace.display(),
                        definition.overlap.as_str(),
                        grant_text(&definition),
                    );
                }
            }
        }
        ScheduleCommand::Show { id, json } => {
            let id = leveler_schedule::ScheduleId::from(id);
            let Some(definition) = store.definition(&id).await? else {
                anyhow::bail!("schedule {id} not found");
            };
            let next = next_run_text(&store, &id).await?;
            let last = store.runs(Some(&id), 5).await?;
            if json {
                print_json(&serde_json::json!({
                    "id": definition.id,
                    "name": definition.name,
                    "workspace": definition.workspace,
                    "action": action_json(&definition.action),
                    "trigger": trigger_json(&definition.trigger),
                    "timezone": trigger_json(&definition.trigger).get("timezone").cloned(),
                    "session_policy": definition.session_policy.as_str(),
                    "overlap": definition.overlap.as_str(),
                    "mode": definition.mode.as_str(),
                    "model": definition.model.as_ref().map(|model| model.to_string()),
                    "timeout_secs": definition.timeout.as_secs(),
                    "misfire": definition.misfire.as_str(),
                    "misfire_grace_secs": definition.misfire_grace.as_secs(),
                    "enabled": definition.enabled,
                    "session_id": definition.session_id,
                    "unattended_grant": grant_json(&definition),
                    "revision": definition.updated_at.to_rfc3339(),
                    "next_occurrence": next,
                    "recent_runs": last,
                }))?;
            } else {
                println!("{}  {}", definition.id, definition.name);
                println!("  workspace:  {}", definition.workspace.display());
                println!("  action:     {}", action_text(&definition.action));
                println!("  trigger:    {}", trigger_text(&definition.trigger));
                println!("  next:       {}", next.unwrap_or_else(|| "none".into()));
                println!(
                    "  state:      {}",
                    if definition.enabled {
                        "enabled"
                    } else {
                        "paused"
                    }
                );
                println!("  permission: {}", definition.mode.as_str());
                println!(
                    "  model:      {}",
                    definition
                        .model
                        .as_ref()
                        .map(|model| model.to_string())
                        .unwrap_or_else(|| "-".into())
                );
                println!("  session:    {}", definition.session_policy.as_str());
                println!(
                    "  bound to:   {}",
                    definition
                        .session_id
                        .as_ref()
                        .map(|session| session.as_str())
                        .unwrap_or("-")
                );
                println!("  overlap:    {}", definition.overlap.as_str());
                println!(
                    "  misfire:    {} (grace {}s)",
                    definition.misfire.as_str(),
                    definition.misfire_grace.as_secs()
                );
                println!("  grant:      {}", grant_text(&definition));
                if !last.is_empty() {
                    println!("  recent runs:");
                    for run in &last {
                        println!("    {}", run_line(run));
                    }
                }
            }
        }
        ScheduleCommand::Update {
            id,
            name,
            workspace,
            command,
            native_action,
            native_input,
            agent_goal,
            at,
            every,
            anchor,
            daily,
            weekly,
            local_time,
            cron,
            timezone,
            mode,
            model,
            session_policy,
            overlap,
            timeout,
            misfire,
            misfire_grace,
            expected_revision,
            json,
        } => {
            let id = leveler_schedule::ScheduleId::from(id);
            let Some(existing) = store.definition(&id).await? else {
                anyhow::bail!("schedule {id} not found");
            };
            let replaces_action =
                command.is_some() || native_action.is_some() || agent_goal.is_some();
            let action = if replaces_action {
                build_action(command, native_action, native_input, agent_goal)?
            } else {
                existing.action.clone()
            };
            let replaces_trigger = at.is_some()
                || every.is_some()
                || daily.is_some()
                || weekly.is_some()
                || cron.is_some();
            let trigger = if replaces_trigger {
                build_trigger(at, every, anchor, daily, weekly, local_time, cron, timezone)?
            } else {
                if timezone.is_some() {
                    anyhow::bail!(
                        "--timezone only applies together with a new calendar rule \
                         (--daily, --weekly or --cron)"
                    );
                }
                existing.trigger.clone()
            };
            let model = match model.as_deref() {
                Some("") => None,
                Some(reference) => Some(
                    ModelRef::parse(reference)
                        .with_context(|| format!("`{reference}` is not a valid model reference"))?,
                ),
                None => existing.model.clone(),
            };
            let expected = expected_revision.as_deref().map(parse_time).transpose()?;
            let definition = store
                .update_definition(
                    &id,
                    DefinitionUpdate {
                        name: name.unwrap_or_else(|| existing.name.clone()),
                        workspace: workspace.unwrap_or_else(|| existing.workspace.clone()),
                        action,
                        trigger,
                        session_policy: session_policy
                            .map(map_session_policy)
                            .unwrap_or(existing.session_policy),
                        overlap: overlap.map(map_overlap_policy).unwrap_or(existing.overlap),
                        mode: mode.map(schedule_mode).unwrap_or(existing.mode),
                        model,
                        timeout: timeout.map(Duration::from_secs).unwrap_or(existing.timeout),
                        misfire: misfire.map(misfire_policy).unwrap_or(existing.misfire),
                        misfire_grace: misfire_grace
                            .map(Duration::from_secs)
                            .unwrap_or(existing.misfire_grace),
                        // A grant survives only while it still covers the new
                        // declaration; the store drops it otherwise.
                        grant: None,
                    },
                    expected,
                )
                .await
                .with_context(|| {
                    format!("updating schedule {id}; re-read it if another editor changed it")
                })?;
            let next = next_run_text(&store, &id).await?;
            if json {
                print_json(&definition_json(&definition, next.as_deref()))?;
            } else {
                println!("updated schedule {}", definition.id);
                println!("  name:       {}", definition.name);
                println!("  action:     {}", action_text(&definition.action));
                println!("  trigger:    {}", trigger_text(&definition.trigger));
                println!("  grant:      {}", grant_text(&definition));
                println!("  next:       {}", next.unwrap_or_else(|| "none".into()));
            }
        }
        ScheduleCommand::Grant {
            id,
            allow_external_side_effects,
            json,
        } => {
            let id = leveler_schedule::ScheduleId::from(id);
            let Some(existing) = store.definition(&id).await? else {
                anyhow::bail!("schedule {id} not found");
            };
            let definition = store
                .update_definition(
                    &id,
                    DefinitionUpdate {
                        name: existing.name.clone(),
                        workspace: existing.workspace.clone(),
                        action: existing.action.clone(),
                        trigger: existing.trigger.clone(),
                        session_policy: existing.session_policy,
                        overlap: existing.overlap,
                        mode: existing.mode,
                        model: existing.model.clone(),
                        timeout: existing.timeout,
                        misfire: existing.misfire,
                        misfire_grace: existing.misfire_grace,
                        grant: Some(UnattendedRequest {
                            allow_external_side_effects,
                        }),
                    },
                    Some(existing.updated_at),
                )
                .await
                .context("issuing the unattended grant")?;
            let next = next_run_text(&store, &id).await?;
            if json {
                print_json(&definition_json(&definition, next.as_deref()))?;
            } else {
                println!("granted unattended execution for schedule {id}");
                println!(
                    "  covers:     {}",
                    if allow_external_side_effects {
                        "actions with external side effects"
                    } else {
                        "actions without external side effects"
                    }
                );
                println!("  grant:      {}", grant_text(&definition));
            }
        }
        ScheduleCommand::Runs {
            definition,
            limit,
            json,
        } => {
            let definition = definition.map(leveler_schedule::ScheduleId::from);
            let runs = store.runs(definition.as_ref(), limit).await?;
            if json {
                print_json(&serde_json::to_value(&runs)?)?;
            } else if runs.is_empty() {
                println!("no runs");
            } else {
                for run in runs {
                    println!("{}", run_line(&run));
                    if let Some(error) = run.error {
                        println!("  reason: {error}");
                    }
                    if run.usage_requests.is_some() {
                        println!(
                            "  usage: requests={} input={} cached={} output={} reasoning={} cost_usd={}",
                            run.usage_requests.unwrap_or(0),
                            run.input_tokens.unwrap_or(0),
                            run.cached_input_tokens.unwrap_or(0),
                            run.output_tokens.unwrap_or(0),
                            run.reasoning_tokens.unwrap_or(0),
                            run.cost_usd_micros
                                .map(|micros| format!("{:.4}", micros as f64 / 1_000_000.0))
                                .unwrap_or_else(|| "-".into()),
                        );
                    }
                }
            }
        }
        ScheduleCommand::Remove { id } => {
            let id = leveler_schedule::ScheduleId::from(id);
            if store.remove_definition(&id).await? {
                println!("removed schedule {id}");
            } else {
                anyhow::bail!("schedule {id} not found");
            }
        }
        ScheduleCommand::Pause { id } => {
            let id = leveler_schedule::ScheduleId::from(id);
            if store.set_enabled(&id, false).await? {
                println!(
                    "paused schedule {id}; future occurrences will not be created. An already \
                     started run keeps running"
                );
            } else {
                anyhow::bail!("schedule {id} not found");
            }
        }
        ScheduleCommand::Resume { id } => {
            let id = leveler_schedule::ScheduleId::from(id);
            if store.set_enabled(&id, true).await? {
                println!("resumed schedule {id}");
            } else {
                anyhow::bail!("schedule {id} not found");
            }
        }
        ScheduleCommand::RunNow { id, json } => {
            let id = leveler_schedule::ScheduleId::from(id);
            let Some(definition) = store.definition(&id).await? else {
                anyhow::bail!("schedule {id} not found");
            };
            let occurrence = store.create_run_now(&definition).await?;
            // Execute this occurrence explicitly. A generic pass could be
            // consumed by an unrelated overdue slot, leaving the requested run
            // pending while the command appears to have succeeded.
            let state = dispatcher(config_dir, store.clone())
                .dispatch_run_now(&occurrence.id)
                .await?;
            let run = store
                .runs(Some(&id), 20)
                .await?
                .into_iter()
                .find(|run| run.occurrence_id == occurrence.id);
            if json {
                print_json(&serde_json::json!({
                    "occurrence": occurrence.id,
                    "scheduled_for": occurrence.scheduled_for,
                    "state": state,
                    "run": run,
                }))?;
            } else {
                match &run {
                    Some(run) => println!("{}", run_line(run)),
                    None => println!(
                        "created a run-now slot {} but it was not claimable (an earlier run \
                         still owns this schedule); a dispatcher will pick it up on its next \
                         wake",
                        occurrence.id
                    ),
                }
                if let Some(error) = run.as_ref().and_then(|run| run.error.as_deref()) {
                    println!("  reason: {error}");
                }
            }
        }
        ScheduleCommand::Cancel { id, json } => {
            let id = leveler_schedule::ScheduleId::from(id);
            let runs = store.request_cancel_definition(&id).await?;
            if json {
                print_json(&serde_json::json!({
                    "definition": id,
                    "cancel_requested_runs": runs.iter().map(|run| run.as_str()).collect::<Vec<_>>(),
                }))?;
            } else if runs.is_empty() {
                println!("schedule {id} has no active run to cancel");
            } else {
                println!(
                    "requested cancellation of {} active run(s) of schedule {id}: {}",
                    runs.len(),
                    runs.iter()
                        .map(|run| run.as_str())
                        .collect::<Vec<_>>()
                        .join(", ")
                );
                println!(
                    "the dispatcher that owns each run issues the runtime cancellation and \
                     records the terminal fact; nothing is reported as cancelled until observed"
                );
            }
        }
        ScheduleCommand::RevokeGrant { id } => {
            let id = leveler_schedule::ScheduleId::from(id);
            if store.revoke_unattended(&id).await? {
                println!(
                    "revoked the unattended grant for schedule {id}; later native and agent \
                     runs are refused before they start"
                );
            } else {
                println!("schedule {id} has no unattended grant to revoke");
            }
        }
        ScheduleCommand::Wakeups { limit, json } => {
            let wakes = store.wakes(limit).await?;
            if json {
                print_json(&serde_json::json!(
                    wakes.iter().map(wake_json).collect::<Vec<_>>()
                ))?;
            } else if wakes.is_empty() {
                println!("no dispatch passes recorded");
            } else {
                for wake in &wakes {
                    println!("{}", wake_line(wake));
                }
            }
        }
        ScheduleCommand::Dispatch { max, source, json } => {
            let summary = dispatcher(config_dir, store)
                .dispatch_due_with_source(max, wake_source(source))
                .await?;
            print_summary(&summary, json)?;
        }
        ScheduleCommand::Serve { max, poll_secs } => {
            serve(config_dir, store, max, poll_secs).await?;
        }
        ScheduleCommand::Enable { json } => {
            let spec = wakeup_spec(&layout, config_dir.as_deref())?;
            let status = wakeup::enable(&spec)
                .await
                .map_err(|error| anyhow::anyhow!("could not enable the OS wakeup: {error}"))?;
            print_wakeup_status(&status, json)?;
        }
        ScheduleCommand::Disable { json } => {
            let status = wakeup::disable(layout.home().root())
                .await
                .map_err(|error| anyhow::anyhow!("could not disable the OS wakeup: {error}"))?;
            print_wakeup_status(&status, json)?;
        }
        ScheduleCommand::Uninstall { json } => {
            let status = wakeup::disable(layout.home().root())
                .await
                .map_err(|error| anyhow::anyhow!("could not remove the OS wakeup: {error}"))?;
            if !json {
                let definitions = store.definitions().await?.len();
                println!(
                    "removed the OS wakeup; {definitions} schedule definition(s) and their \
                     history were kept"
                );
            }
            print_wakeup_status(&status, json)?;
        }
        ScheduleCommand::Status { json } => {
            let status = wakeup::status(layout.home().root())
                .await
                .map_err(|error| anyhow::anyhow!("could not read the OS wakeup: {error}"))?;
            let definitions = store.definitions().await?;
            let next = store.next_due().await?;
            let overdue = store.overdue_pending(leveler_core::now()).await?;
            let last_os_wake = store.last_os_wake().await?;
            let value = serde_json::json!({
                "wakeup": wakeup_status_json(&status),
                "definitions": definitions.len(),
                "next_occurrence": next.map(|next| next.to_rfc3339()),
                "overdue_pending": overdue,
                "last_os_wake": last_os_wake.as_ref().map(wake_json),
            });
            if json {
                print_json(&value)?;
            } else {
                print_wakeup_status(&status, false)?;
                println!("schedules: {} definition(s)", definitions.len());
                println!(
                    "next:      {}",
                    next.map(|next| next.to_rfc3339())
                        .unwrap_or_else(|| "none".into())
                );
                println!("overdue:   {overdue} pending slot(s)");
                match &last_os_wake {
                    Some(wake) => println!("last OS wake: {}", wake_line(wake)),
                    None => println!("last OS wake: none recorded"),
                }
            }
        }
        ScheduleCommand::Doctor { json } => {
            let status = wakeup::status(layout.home().root())
                .await
                .map_err(|error| anyhow::anyhow!("could not read the OS wakeup: {error}"))?;
            let spec = wakeup_spec(&layout, config_dir.as_deref()).ok();
            let definitions = store.definitions().await?;
            let overdue = store.overdue_pending(leveler_core::now()).await?;
            let last_os_wake = store.last_os_wake().await?;
            let report = doctor_report(
                &status,
                spec.as_ref(),
                definitions.len(),
                overdue,
                last_os_wake.as_ref(),
            );
            if json {
                print_json(&serde_json::json!({
                    "healthy": report.healthy,
                    "checks": report.checks.iter().map(check_json).collect::<Vec<_>>(),
                }))?;
            } else {
                for check in &report.checks {
                    println!(
                        "[{}] {}: {}",
                        check.severity.as_str(),
                        check.name,
                        check.detail
                    );
                }
                println!(
                    "{}",
                    if report.healthy {
                        "schedule doctor: no blocking problem found"
                    } else {
                        "schedule doctor: at least one check failed"
                    }
                );
            }
            if !report.healthy {
                anyhow::bail!("schedule doctor found at least one failed check");
            }
        }
    }
    Ok(())
}

/// The next run a dispatcher could claim for this definition.
///
/// The store keeps terminal occurrences for history; none of them is a next
/// run. This projection reads the same pending slot the manager, the TUI and
/// the agent tools display, so no surface has to reinterpret occurrence rows.
async fn next_run_text(
    store: &ScheduleStore,
    id: &leveler_schedule::ScheduleId,
) -> anyhow::Result<Option<String>> {
    Ok(store
        .next_pending_occurrence(id)
        .await?
        .map(|occurrence| occurrence.scheduled_for.to_rfc3339()))
}

async fn serve(
    config_dir: Option<PathBuf>,
    store: ScheduleStore,
    max: usize,
    poll_secs: u64,
) -> anyhow::Result<()> {
    let dispatcher = dispatcher(config_dir, store);
    let poll = Duration::from_secs(poll_secs.max(1));
    let shutdown = tokio::signal::ctrl_c();
    tokio::pin!(shutdown);
    loop {
        let summary = dispatcher
            .dispatch_due_with_source(max, WakeSource::Serve)
            .await?;
        if summary.claimed > 0 || summary.recovered > 0 {
            print_summary(&summary, false)?;
        }
        let next = dispatcher.next_due().await?;
        let now = Utc::now();
        let sleep = match next {
            Some(next) if next > now => (next - now).to_std().unwrap_or(poll).min(poll),
            _ => poll,
        };
        tokio::select! {
            _ = &mut shutdown => {
                println!("stopping the schedule dispatcher");
                return Ok(());
            }
            _ = tokio::time::sleep(sleep.max(Duration::from_millis(200))) => {}
        }
    }
}

fn dispatcher(config_dir: Option<PathBuf>, store: ScheduleStore) -> Dispatcher {
    let connector = DaemonConnector {
        config_dir: config_dir.clone(),
        launch: DetachedRuntimeLaunch {
            executable: std::env::current_exe().unwrap_or_else(|_| PathBuf::from("leveler")),
            ready_prefix: "leveler-schedule-ready".into(),
        },
    };
    Dispatcher::new(store, Arc::new(connector))
        .with_native_executor(Arc::new(NativeExecutor::new()))
        .with_usage_reader(Arc::new(SessionUsage { config_dir }))
        .with_lease(DISPATCH_LEASE)
}

/// The scheduled native path is the same `NativeActions::run` the interactive
/// tool and `leveler native` use. Only the admission mode differs: an
/// unattended run may never prompt, so a missing permission blocks it instead
/// of waiting for a person who is not there.
struct NativeExecutor {
    runner: CommandRunner,
    native: NativeActions,
}

impl NativeExecutor {
    fn new() -> Self {
        Self {
            runner: CommandRunner::new(),
            native: NativeActions::new(),
        }
    }
}

#[async_trait]
impl NativeActionExecutor for NativeExecutor {
    fn classify(&self, action: &str) -> Result<NativeActionClass, String> {
        let action = ActionId::parse(action)?;
        if !action.unattended_after_preconditions() {
            return Err(format!(
                "{} needs a person to complete it in the application; a scheduled run \
                 cannot supply that person",
                action.as_str()
            ));
        }
        Ok(NativeActionClass {
            external_side_effects: action.external_side_effects(),
        })
    }

    async fn execute(
        &self,
        request: NativeExecutionRequest,
    ) -> Result<NativeExecutionReport, String> {
        let action = ActionId::parse(&request.action)?;
        let run = match self
            .native
            .run(
                &self.runner,
                &NativeActionRequest {
                    action,
                    input: request.input,
                },
                PermissionMode::Unattended,
                Some(request.timeout),
                256 * 1024,
                request.cancellation,
            )
            .await
        {
            Ok(run) => run,
            // `run` returns an error only while validating or preparing,
            // before any process starts. Reporting `Unknown` here would claim
            // an effect that provably does not exist.
            Err(error) => {
                let status = match &error {
                    leveler_native::NativeError::InvalidInput(_) => NativeRunStatus::InvalidInput,
                    leveler_native::NativeError::Unsupported(..) => NativeRunStatus::Unsupported,
                    leveler_native::NativeError::Prepare { .. } => NativeRunStatus::NotStarted,
                };
                return Ok(NativeExecutionReport {
                    status,
                    provider: None,
                    execution_id: None,
                    exit_code: None,
                    detail: error.to_string(),
                    capability_state: None,
                    permission_state: None,
                    executed: false,
                });
            }
        };
        let executed = !run.outcome.execution_id.is_empty();
        Ok(NativeExecutionReport {
            status: match run.outcome.status {
                ActionStatus::Completed => NativeRunStatus::Completed,
                ActionStatus::Submitted => NativeRunStatus::Submitted,
                ActionStatus::Unknown => NativeRunStatus::Unknown,
                ActionStatus::RequiresPermission => NativeRunStatus::RequiresPermission,
                ActionStatus::RequiresConfiguration => NativeRunStatus::RequiresConfiguration,
                ActionStatus::Failed => NativeRunStatus::Failed,
                ActionStatus::Unsupported => NativeRunStatus::Unsupported,
            },
            provider: Some(run.provider.as_str().to_string()),
            execution_id: executed.then(|| run.outcome.execution_id.clone()),
            exit_code: run.outcome.exit_code,
            detail: run.outcome.detail,
            capability_state: run
                .capability
                .as_ref()
                .map(|capability| capability.state.as_str().to_string()),
            permission_state: run
                .capability
                .as_ref()
                .map(|capability| capability.permission.as_str().to_string()),
            executed,
        })
    }
}

/// Model usage from the authoritative request ledger, never from the model's
/// own text. The dispatcher subtracts this baseline from the final totals so a
/// continue-session run is not credited with earlier turns.
struct SessionUsage {
    config_dir: Option<PathBuf>,
}

#[async_trait]
impl UsageReader for SessionUsage {
    async fn usage(&self, workspace: &Path, session: &SessionId) -> Result<UsageSnapshot, String> {
        let layout = Layout::resolve(workspace.to_path_buf(), self.config_dir.clone());
        let path = layout.database_path();
        // A workspace with no database has no recorded model requests. That is
        // zero usage, not an unobserved baseline.
        if !path.is_file() {
            return Ok(UsageSnapshot::default());
        }
        let db = leveler_storage::Database::connect(&path)
            .await
            .map_err(|error| format!("could not open {}: {error}", path.display()))?;
        let totals = leveler_storage::ModelRequestRepository::new(&db)
            .reconcile_session(session)
            .await
            .map_err(|error| format!("could not read model usage: {error}"))?
            .total;
        Ok(UsageSnapshot {
            requests: totals.requests,
            input_tokens: totals.input_tokens,
            cached_input_tokens: totals.cached_input_tokens,
            output_tokens: totals.output_tokens,
            reasoning_tokens: totals.reasoning_tokens,
            cost_usd_micros: totals.cost_usd_micros,
            rows_without_cost: totals.rows_without_cost,
        })
    }
}

struct DaemonConnector {
    config_dir: Option<PathBuf>,
    launch: DetachedRuntimeLaunch,
}

#[async_trait]
impl RuntimeConnector for DaemonConnector {
    async fn connect(&self, workspace: &Path) -> Result<Arc<dyn LocalRuntimeService>, String> {
        let layout = Layout::resolve(workspace.to_path_buf(), self.config_dir.clone());
        let client = ensure_default_runtime(&layout, &self.launch, NonInteractiveHandoffUi::new())
            .await
            .map_err(|error| error.to_string())?;
        Ok(Arc::new(client))
    }
}

fn parse_time(raw: &str) -> anyhow::Result<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(raw)
        .map(|time| time.with_timezone(&Utc))
        .with_context(|| format!("`{raw}` is not an RFC3339 instant"))
}

/// The schedule domain stores the wire permission profile. The CLI's
/// `RunMode` is the user-facing spelling; mapping it here keeps the two
/// vocabularies separate instead of reinterpreting an execution profile.
fn schedule_mode(mode: RunMode) -> PermissionProfile {
    match mode {
        RunMode::RequestApproval => PermissionProfile::RequestApproval,
        RunMode::Assisted => PermissionProfile::Assisted,
        RunMode::FullAccess => PermissionProfile::FullAccess,
    }
}

fn map_session_policy(policy: ScheduleSessionPolicy) -> SessionPolicy {
    match policy {
        ScheduleSessionPolicy::Continue => SessionPolicy::Continue,
        ScheduleSessionPolicy::Fresh => SessionPolicy::Fresh,
    }
}

fn map_overlap_policy(policy: ScheduleOverlap) -> OverlapPolicy {
    match policy {
        ScheduleOverlap::SkipIfRunning => OverlapPolicy::SkipIfRunning,
        ScheduleOverlap::QueueOne => OverlapPolicy::QueueOne,
        ScheduleOverlap::AllowConcurrent => OverlapPolicy::AllowConcurrent,
    }
}

/// One and only one action kind; clap enforces it, this also gives the store
/// typed input instead of a command string reinterpreted as another kind.
fn build_action(
    command: Option<String>,
    native_action: Option<String>,
    native_input: Option<String>,
    agent_goal: Option<String>,
) -> anyhow::Result<ScheduleAction> {
    match (command, native_action, agent_goal) {
        (Some(command), None, None) => Ok(ScheduleAction::Shell { command }),
        (None, Some(action), None) => {
            ActionId::parse(&action).map_err(anyhow::Error::msg)?;
            let input = match native_input {
                Some(raw) => serde_json::from_str(&raw)
                    .with_context(|| format!("--native-input is not valid JSON: {raw}"))?,
                None => serde_json::json!({}),
            };
            if !input.is_object() {
                anyhow::bail!("--native-input must be a JSON object");
            }
            Ok(ScheduleAction::Native { action, input })
        }
        (None, None, Some(goal)) => Ok(ScheduleAction::Agent { goal }),
        _ => anyhow::bail!("pass exactly one of --command, --native-action or --agent-goal"),
    }
}

/// One and only one trigger rule. `add` and `update` share this builder so a
/// rule means the same thing on both commands; the argument order matches the
/// flag names rather than the struct fields.
#[allow(clippy::too_many_arguments)]
fn build_trigger(
    at: Option<String>,
    every: Option<u64>,
    anchor: Option<String>,
    daily: Option<String>,
    weekly: Option<String>,
    local_time: Option<String>,
    cron: Option<String>,
    timezone: Option<String>,
) -> anyhow::Result<ScheduleTrigger> {
    match (at, every, daily, weekly, cron) {
        (Some(at), None, None, None, None) => {
            if timezone.is_some() {
                anyhow::bail!("--timezone only applies to --daily, --weekly or --cron");
            }
            Ok(ScheduleTrigger::Once {
                at: parse_time(&at)?,
            })
        }
        (None, Some(every), None, None, None) => {
            if timezone.is_some() {
                anyhow::bail!("--timezone only applies to --daily, --weekly or --cron");
            }
            Ok(ScheduleTrigger::Every {
                anchor: anchor
                    .as_deref()
                    .map(parse_time)
                    .transpose()?
                    .unwrap_or_else(Utc::now),
                every: Duration::from_secs(every),
            })
        }
        (None, None, Some(time), None, None) => {
            let timezone = rule_timezone(timezone.as_deref())?;
            Ok(ScheduleTrigger::Daily {
                time: LocalTime::parse(&time).map_err(anyhow::Error::msg)?,
                timezone,
            })
        }
        (None, None, None, Some(days), None) => {
            let timezone = rule_timezone(timezone.as_deref())?;
            let time = local_time
                .as_deref()
                .context("--weekly needs --local-time <HH:MM> for the wall-clock run time")?;
            Ok(ScheduleTrigger::Weekly {
                weekdays: WeekdaySet::parse(&days).map_err(anyhow::Error::msg)?,
                time: LocalTime::parse(time).map_err(anyhow::Error::msg)?,
                timezone,
            })
        }
        (None, None, None, None, Some(expression)) => {
            let timezone = rule_timezone(timezone.as_deref())?;
            parse_cron(&expression).map_err(anyhow::Error::msg)?;
            Ok(ScheduleTrigger::Cron {
                expression,
                timezone,
            })
        }
        (None, None, None, None, None) => {
            anyhow::bail!(
                "a schedule needs --at <RFC3339>, --every <SECONDS>, --daily <HH:MM>, \
                 --weekly <DAYS> or --cron <EXPRESSION>"
            )
        }
        _ => unreachable!("clap makes the trigger flags mutually exclusive"),
    }
}

fn action_kind_text(action: &ScheduleAction) -> &'static str {
    action.kind().as_str()
}

fn action_text(action: &ScheduleAction) -> String {
    match action {
        ScheduleAction::Shell { command } => format!("shell: {command}"),
        ScheduleAction::Native { action, input } => {
            format!("native: {action} (input {input})")
        }
        ScheduleAction::Agent { goal } => format!("agent: {goal}"),
    }
}

fn action_json(action: &ScheduleAction) -> serde_json::Value {
    match action {
        ScheduleAction::Shell { command } => serde_json::json!({
            "kind": "shell",
            "command": command,
        }),
        ScheduleAction::Native { action, input } => serde_json::json!({
            "kind": "native",
            "action": action,
            "input": input,
        }),
        ScheduleAction::Agent { goal } => serde_json::json!({
            "kind": "agent",
            "goal": goal,
        }),
    }
}

/// The shape `add`, `update` and `grant` print. The revision is included so a
/// caller can use it as `--expected-revision` on the next edit.
fn definition_json(definition: &ScheduleDefinition, next: Option<&str>) -> serde_json::Value {
    serde_json::json!({
        "id": definition.id,
        "name": definition.name,
        "workspace": definition.workspace,
        "action": action_json(&definition.action),
        "trigger": trigger_json(&definition.trigger),
        "session_policy": definition.session_policy.as_str(),
        "overlap": definition.overlap.as_str(),
        "mode": definition.mode.as_str(),
        "model": definition.model.as_ref().map(|model| model.to_string()),
        "timeout_secs": definition.timeout.as_secs(),
        "misfire": definition.misfire.as_str(),
        "misfire_grace_secs": definition.misfire_grace.as_secs(),
        "unattended_grant": grant_json(definition),
        "revision": definition.updated_at.to_rfc3339(),
        "next_occurrence": next,
    })
}

fn grant_json(definition: &leveler_schedule::ScheduleDefinition) -> serde_json::Value {
    match &definition.unattended {
        Some(grant) => serde_json::json!({
            "version": grant.version,
            "definition_id": grant.definition_id,
            "workspace": grant.workspace,
            "permission_mode": grant.permission_mode,
            "actions": grant.actions.iter().map(|kind| kind.as_str()).collect::<Vec<_>>(),
            "external_side_effects": grant.external_side_effects,
            "granted_at": grant.granted_at,
            "requires_regrant": !definition.unattended_grant_valid(),
        }),
        // The permission profile is never a durable authorization. A missing
        // grant must stay missing here, whatever interactive profile the
        // definition runs under.
        None => serde_json::Value::Null,
    }
}

fn grant_text(definition: &leveler_schedule::ScheduleDefinition) -> String {
    match &definition.unattended {
        Some(grant) => {
            let grant = format!(
                "unattended {} ({}; external side effects: {})",
                grant
                    .actions
                    .iter()
                    .map(|kind| kind.as_str())
                    .collect::<Vec<_>>()
                    .join(","),
                grant.permission_mode,
                if grant.external_side_effects {
                    "yes"
                } else {
                    "no"
                }
            );
            if definition.unattended_grant_valid() {
                grant
            } else {
                // The dispatcher refuses this grant before starting a run, so
                // saying only "unattended ..." would hide the real state.
                format!("{grant} — no longer matches; unattended runs are refused until re-authorized")
            }
        }
        None => "none — every unattended run (shell, native, agent) is refused before starting; authorize it explicitly".into(),
    }
}

fn run_line(run: &leveler_schedule::ScheduleRun) -> String {
    let identity = match (&run.session_id, run.task_sequence) {
        (Some(session), Some(sequence)) => format!(" session={session} task_seq={sequence}"),
        (Some(session), None) => format!(" session={session}"),
        (None, Some(sequence)) => format!(" task_seq={sequence}"),
        (None, None) => String::new(),
    };
    format!(
        "{}  {}  {}  slot={}  exit={}{}{}",
        run.id,
        run.definition_id,
        run.state.as_str(),
        run.scheduled_for.to_rfc3339(),
        run.exit_code
            .map(|code| code.to_string())
            .unwrap_or_else(|| "-".into()),
        identity,
        match (&run.task_outcome, &run.native_outcome) {
            (Some(outcome), _) => format!("  task={outcome}"),
            (None, Some(outcome)) => format!("  native={outcome}"),
            (None, None) => String::new(),
        },
    )
}

fn trigger_text(trigger: &ScheduleTrigger) -> String {
    trigger.describe()
}

fn trigger_json(trigger: &ScheduleTrigger) -> serde_json::Value {
    match trigger {
        ScheduleTrigger::Once { at } => serde_json::json!({
            "kind": "once",
            "at": at,
        }),
        ScheduleTrigger::Every { anchor, every } => serde_json::json!({
            "kind": "every",
            "anchor": anchor,
            "every_secs": every.as_secs(),
        }),
        ScheduleTrigger::Daily { time, timezone } => serde_json::json!({
            "kind": "daily",
            "time": time.to_string(),
            "timezone": timezone.to_string(),
        }),
        ScheduleTrigger::Weekly {
            weekdays,
            time,
            timezone,
        } => serde_json::json!({
            "kind": "weekly",
            "weekdays": weekdays.to_string(),
            "time": time.to_string(),
            "timezone": timezone.to_string(),
        }),
        ScheduleTrigger::Cron {
            expression,
            timezone,
        } => serde_json::json!({
            "kind": "cron",
            "expression": expression,
            "timezone": timezone.to_string(),
        }),
    }
}

/// Resolve the wall-clock timezone for calendar rules. An explicit `--timezone`
/// wins; otherwise use the host's current IANA zone, never a UTC assumption.
fn rule_timezone(explicit: Option<&str>) -> anyhow::Result<chrono_tz::Tz> {
    match explicit {
        Some(raw) => leveler_schedule::parse_timezone(raw).map_err(anyhow::Error::msg),
        None => {
            let zone = iana_time_zone::get_timezone().map_err(|error| {
                anyhow::anyhow!(
                    "could not determine the system timezone ({error}); pass --timezone"
                )
            })?;
            leveler_schedule::parse_timezone(&zone).map_err(anyhow::Error::msg)
        }
    }
}

fn summary_json(summary: &DispatchSummary) -> serde_json::Value {
    serde_json::json!({
        "recovered": summary.recovered,
        "skipped": summary.skipped,
        "missed": summary.missed,
        "claimed": summary.claimed,
        "succeeded": summary.succeeded,
        "submitted": summary.submitted,
        "failed": summary.failed,
        "unknown": summary.unknown,
        "handed_off": summary.handed_off,
        "cancelled": summary.cancelled,
        "waiting_approval": summary.waiting_approval,
        "blocked": summary.blocked,
    })
}

fn print_summary(summary: &DispatchSummary, json: bool) -> anyhow::Result<()> {
    if json {
        return print_json(&summary_json(summary));
    }
    println!(
        "recovered={} skipped={} missed={} claimed={} succeeded={} submitted={} failed={} \
         unknown={} handed_off={} cancelled={} waiting_approval={} blocked={}",
        summary.recovered,
        summary.skipped,
        summary.missed,
        summary.claimed,
        summary.succeeded,
        summary.submitted,
        summary.failed,
        summary.unknown,
        summary.handed_off,
        summary.cancelled,
        summary.waiting_approval,
        summary.blocked,
    );
    Ok(())
}

fn misfire_policy(misfire: ScheduleMisfire) -> MisfirePolicy {
    match misfire {
        ScheduleMisfire::Skip => MisfirePolicy::Skip,
        ScheduleMisfire::RunOnce => MisfirePolicy::RunOnceOnRecovery,
    }
}

fn wake_source(source: ScheduleSource) -> WakeSource {
    match source {
        ScheduleSource::Manual => WakeSource::Manual,
        ScheduleSource::Os => WakeSource::Os,
        ScheduleSource::Serve => WakeSource::Serve,
    }
}

/// The registration the OS backend should hold for this build and home.
///
/// Every field is deterministic: the executable is an absolute path, the
/// config directory is resolved now (the OS job does not inherit the shell's
/// environment) and only non-secret variables are copied.
fn wakeup_spec(layout: &Layout, config_dir: Option<&Path>) -> anyhow::Result<WakeupSpec> {
    let home = layout.home().root().to_path_buf();
    let mut arguments = vec![
        "schedule".into(),
        "dispatch".into(),
        "--source".into(),
        "os".into(),
    ];
    // The home travels in the arguments, not only in the environment: Task
    // Scheduler has no environment map, and an explicit argument is what makes
    // doctor compare the exact home the job was enabled for.
    arguments.push("--home".into());
    arguments.push(home.to_string_lossy().into_owned());
    if let Some(config_dir) = effective_config_dir(config_dir)? {
        arguments.push("--config-dir".into());
        arguments.push(config_dir.to_string_lossy().into_owned());
    }
    // The OS job's stdout is its log. One JSON line per pass is the most
    // parseable record and does not depend on a terminal.
    arguments.push("--json".into());
    Ok(WakeupSpec {
        home: home.clone(),
        executable: wakeup_executable()?,
        arguments,
        environment: wakeup_environment(&home),
        interval: WAKE_INTERVAL,
        log_dir: layout.home().schedule_logs_dir(),
    })
}

/// The config directory the invoking process actually used: an explicit
/// `--config-dir`, else `LEVELER_CONFIG_DIR`. `None` keeps the dispatcher's
/// per-workspace default.
fn effective_config_dir(config_dir: Option<&Path>) -> anyhow::Result<Option<PathBuf>> {
    let Some(config_dir) = config_dir.map(Path::to_path_buf).or_else(|| {
        leveler_core::environment()
            .var_os("LEVELER_CONFIG_DIR")
            .map(PathBuf::from)
    }) else {
        return Ok(None);
    };
    Ok(Some(absolute_path(&config_dir)?))
}

fn absolute_path(path: &Path) -> anyhow::Result<PathBuf> {
    if path.is_absolute() {
        return Ok(path.to_path_buf());
    }
    Ok(std::env::current_dir()
        .context("resolving a relative path against the current directory")?
        .join(path))
}

/// Prefer the Homebrew entry point when it is the same file as the running
/// executable, so a `brew upgrade` that replaces the Cellar target does not
/// leave the OS job pointing at a removed path.
fn wakeup_executable() -> anyhow::Result<PathBuf> {
    let current = std::env::current_exe().context("resolving the leveler executable")?;
    #[cfg(target_os = "macos")]
    if let Ok(resolved) = std::fs::canonicalize(&current) {
        for candidate in ["/opt/homebrew/bin/leveler", "/usr/local/bin/leveler"] {
            let candidate = PathBuf::from(candidate);
            if std::fs::canonicalize(&candidate).ok().as_ref() == Some(&resolved) {
                return Ok(candidate);
            }
        }
    }
    Ok(current)
}

/// Non-secret environment the OS job needs. The OS does not inherit the
/// shell's environment, so the home and config paths must travel with the
/// registration.
fn wakeup_environment(home: &Path) -> BTreeMap<String, String> {
    let snapshot = leveler_core::environment();
    let mut environment = BTreeMap::new();
    for key in ["PATH", "HOME", "TMPDIR", "LEVELER_DAEMON_IDLE_TIMEOUT_SECS"] {
        if let Some(value) = snapshot
            .var_os(key)
            .and_then(|value| value.to_str().map(str::to_owned))
        {
            environment.insert(key.to_owned(), value);
        }
    }
    environment.insert("LEVELER_HOME".into(), home.to_string_lossy().into_owned());
    environment
}

fn wakeup_status_json(status: &WakeupStatus) -> serde_json::Value {
    let registration = status.registration.as_ref();
    serde_json::json!({
        "backend": status.backend,
        "supported": status.supported,
        "label": status.label,
        "registration_path": status.registration_path,
        "registered": status.registered,
        "loaded": status.loaded,
        "interval_secs": registration
            .and_then(|registration| registration.interval)
            .map(|interval| interval.as_secs()),
        "executable": registration
            .map(|registration| registration.executable.display().to_string()),
        "arguments": registration.map(|registration| registration.arguments.clone()),
        "environment": registration.map(|registration| registration.environment.clone()),
        "registration_working_directory": registration
            .and_then(|registration| registration.working_directory.as_ref())
            .map(|path| path.display().to_string()),
        "last_exit_code": status.last_exit_code,
        "run_count": status.run_count,
        "detail": status.detail,
    })
}

fn print_wakeup_status(status: &WakeupStatus, json: bool) -> anyhow::Result<()> {
    if json {
        return print_json(&wakeup_status_json(status));
    }
    if !status.supported {
        println!(
            "OS wakeup: not supported on this platform (backend: {})",
            status.backend
        );
        return Ok(());
    }
    let state = if status.loaded {
        "enabled"
    } else if status.registered {
        "registered, not loaded"
    } else {
        "not registered"
    };
    println!("OS wakeup: {state} ({} {})", status.backend, status.label);
    if let Some(path) = &status.registration_path {
        println!("record:    {}", path.display());
    }
    if let Some(registration) = &status.registration {
        println!(
            "command:   {} {}",
            registration.executable.display(),
            registration.arguments.join(" ")
        );
        if let Some(interval) = registration.interval {
            println!("interval:  {}s", interval.as_secs());
        }
    }
    match (status.last_exit_code, status.run_count) {
        (Some(exit), Some(runs)) => println!("last:      exit {exit}, {runs} run(s)"),
        (Some(exit), None) => println!("last:      exit {exit}"),
        (None, Some(runs)) => println!("runs:      {runs}"),
        (None, None) => {}
    }
    if let Some(detail) = &status.detail {
        println!("detail:    {detail}");
    }
    Ok(())
}

fn wake_json(wake: &leveler_schedule::WakeRecord) -> serde_json::Value {
    serde_json::json!({
        "id": wake.id,
        "source": wake.source.as_str(),
        "started_at": wake.started_at,
        "finished_at": wake.finished_at,
        "recovered": wake.recovered,
        "skipped": wake.skipped,
        "missed": wake.missed,
        "claimed": wake.claimed,
        "succeeded": wake.succeeded,
        "submitted": wake.submitted,
        "failed": wake.failed,
        "unknown": wake.unknown,
        "handed_off": wake.handed_off,
        "cancelled": wake.cancelled,
        "waiting_approval": wake.waiting_approval,
        "blocked": wake.blocked,
        "error": wake.error,
    })
}

fn wake_line(wake: &leveler_schedule::WakeRecord) -> String {
    format!(
        "#{} {} started={} finished={} recovered={} skipped={} missed={} claimed={} \
         succeeded={} submitted={} failed={} unknown={} handed_off={} cancelled={} \
         waiting_approval={} blocked={}{}",
        wake.id,
        wake.source.as_str(),
        wake.started_at.to_rfc3339(),
        wake.finished_at
            .map(|finished| finished.to_rfc3339())
            .unwrap_or_else(|| "unfinished".into()),
        wake.recovered,
        wake.skipped,
        wake.missed,
        wake.claimed,
        wake.succeeded,
        wake.submitted,
        wake.failed,
        wake.unknown,
        wake.handed_off,
        wake.cancelled,
        wake.waiting_approval,
        wake.blocked,
        wake.error
            .as_deref()
            .map(|error| format!(" error={error}"))
            .unwrap_or_default(),
    )
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CheckSeverity {
    Ok,
    Warn,
    Fail,
}

impl CheckSeverity {
    fn as_str(self) -> &'static str {
        match self {
            Self::Ok => "ok",
            Self::Warn => "warn",
            Self::Fail => "fail",
        }
    }
}

#[derive(Debug, Clone)]
struct Check {
    name: &'static str,
    severity: CheckSeverity,
    detail: String,
}

#[derive(Debug, Clone)]
struct DoctorReport {
    healthy: bool,
    checks: Vec<Check>,
}

fn check_json(check: &Check) -> serde_json::Value {
    serde_json::json!({
        "check": check.name,
        "severity": check.severity.as_str(),
        "detail": check.detail,
    })
}

/// Read-only consistency check. The OS registration is opt-in, so a missing
/// registration is a warning even when schedules exist; a record that exists
/// but disagrees with this build's deterministic command is a failure, because
/// the next wake would run a different executable or configuration than the
/// user enabled.
fn doctor_report(
    status: &WakeupStatus,
    spec: Option<&WakeupSpec>,
    definitions: usize,
    overdue: usize,
    last_os_wake: Option<&leveler_schedule::WakeRecord>,
) -> DoctorReport {
    let mut checks = Vec::new();
    let mut push = |name: &'static str, severity: CheckSeverity, detail: String| {
        checks.push(Check {
            name,
            severity,
            detail,
        })
    };

    if status.supported {
        push(
            "platform",
            CheckSeverity::Ok,
            format!("{} is available", status.backend),
        );
    } else {
        push(
            "platform",
            CheckSeverity::Fail,
            format!("OS wakeup is not supported by backend `{}`", status.backend),
        );
    }

    if status.supported {
        if definitions == 0 {
            if status.registered {
                push(
                    "registration",
                    CheckSeverity::Warn,
                    "an OS wakeup is registered but no schedule is defined".into(),
                );
            } else {
                push(
                    "registration",
                    CheckSeverity::Ok,
                    "no schedule is defined, so no OS wakeup is required".into(),
                );
            }
        } else if !status.registered {
            push(
                "registration",
                CheckSeverity::Warn,
                format!(
                    "{definitions} schedule definition(s) exist but no OS wakeup is \
                     registered; they run only while a dispatcher is started. Run \
                     `leveler schedule enable` for unattended wakeups"
                ),
            );
        } else if !status.loaded {
            push(
                "registration",
                CheckSeverity::Fail,
                "the OS record exists but is not loaded; run `leveler schedule enable`".into(),
            );
        } else {
            push(
                "registration",
                CheckSeverity::Ok,
                format!("{} is registered and loaded", status.label),
            );
        }

        match (spec, status.registration.as_ref()) {
            (Some(spec), Some(registration)) => {
                if registration.executable == spec.executable {
                    push(
                        "executable",
                        CheckSeverity::Ok,
                        spec.executable.display().to_string(),
                    );
                } else {
                    push(
                        "executable",
                        CheckSeverity::Fail,
                        format!(
                            "registered `{}` but this build is `{}`; run `leveler schedule \
                             enable` to re-register",
                            registration.executable.display(),
                            spec.executable.display()
                        ),
                    );
                }
                if !registration.executable.is_file() {
                    push(
                        "executable_exists",
                        CheckSeverity::Fail,
                        format!(
                            "the registered executable `{}` no longer exists",
                            registration.executable.display()
                        ),
                    );
                }
                if registration.arguments == spec.arguments {
                    push("arguments", CheckSeverity::Ok, spec.arguments.join(" "));
                } else {
                    push(
                        "arguments",
                        CheckSeverity::Fail,
                        format!(
                            "registered `{}` but this build would use `{}`",
                            registration.arguments.join(" "),
                            spec.arguments.join(" ")
                        ),
                    );
                }
                if registration.interval == Some(spec.interval) {
                    push(
                        "interval",
                        CheckSeverity::Ok,
                        format!("{}s", spec.interval.as_secs()),
                    );
                } else {
                    push(
                        "interval",
                        CheckSeverity::Fail,
                        format!(
                            "registered {:?} but this build expects {}s",
                            registration.interval.map(|interval| interval.as_secs()),
                            spec.interval.as_secs()
                        ),
                    );
                }
                let mismatched: Vec<_> = spec
                    .environment
                    .iter()
                    .filter(|(key, value)| registration.environment.get(*key) != Some(*value))
                    .map(|(key, _)| key.clone())
                    .collect();
                if mismatched.is_empty() {
                    push(
                        "environment",
                        CheckSeverity::Ok,
                        format!("{} variable(s) match", spec.environment.len()),
                    );
                } else {
                    push(
                        "environment",
                        CheckSeverity::Fail,
                        format!(
                            "registered environment differs for {}; run `leveler schedule \
                             enable` to re-register",
                            mismatched.join(", ")
                        ),
                    );
                }
            }
            (Some(_), None) if status.registered => push(
                "registration",
                CheckSeverity::Fail,
                "the OS record exists but its contents could not be read".into(),
            ),
            (None, _) => push(
                "executable",
                CheckSeverity::Fail,
                "the leveler executable could not be resolved for this registration".into(),
            ),
            (Some(_), None) => {}
        }
    }

    if overdue == 0 {
        push(
            "overdue",
            CheckSeverity::Ok,
            "no due slot is waiting for a dispatcher".into(),
        );
    } else {
        push(
            "overdue",
            CheckSeverity::Warn,
            format!(
                "{overdue} due slot(s) are pending; the next wake will dispatch them or \
                 apply the misfire policy"
            ),
        );
    }

    match last_os_wake {
        Some(wake) if wake.finished_at.is_none() => push(
            "last_os_wake",
            CheckSeverity::Warn,
            format!(
                "wake #{} started {} and never finished; the driver may have been stopped \
                 mid-pass",
                wake.id,
                wake.started_at.to_rfc3339()
            ),
        ),
        Some(wake) => push("last_os_wake", CheckSeverity::Ok, wake_line(wake)),
        None => push(
            "last_os_wake",
            CheckSeverity::Warn,
            "no OS wake has been recorded yet".into(),
        ),
    }

    DoctorReport {
        healthy: checks
            .iter()
            .all(|check| check.severity != CheckSeverity::Fail),
        checks,
    }
}

fn print_json(value: &serde_json::Value) -> anyhow::Result<()> {
    println!("{}", serde_json::to_string_pretty(value)?);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use leveler_schedule::{ActionKind, UnattendedGrant};

    fn definition_with_grant() -> ScheduleDefinition {
        let action = ScheduleAction::Shell {
            command: "printf x".into(),
        };
        let trigger = ScheduleTrigger::Every {
            anchor: Utc::now(),
            every: Duration::from_secs(3600),
        };
        let id = leveler_schedule::ScheduleId::from("00000000-0000-4000-8000-000000000001");
        let workspace = PathBuf::from("/tmp/schedule-display");
        let grant = UnattendedGrant::new(
            id.clone(),
            workspace.clone(),
            PermissionProfile::Assisted,
            &action,
            &trigger,
            None,
            UnattendedRequest {
                allow_external_side_effects: true,
            },
            Utc::now(),
        )
        .expect("a grant for this definition");
        ScheduleDefinition {
            id,
            name: "probe".into(),
            workspace,
            action,
            trigger,
            session_policy: SessionPolicy::Continue,
            overlap: OverlapPolicy::SkipIfRunning,
            mode: PermissionProfile::Assisted,
            model: None,
            timeout: Duration::from_secs(600),
            misfire: MisfirePolicy::Skip,
            misfire_grace: Duration::from_secs(300),
            session_id: None,
            unattended: Some(grant),
            enabled: true,
            created_at: Utc::now(),
            updated_at: Utc::now(),
        }
    }

    #[test]
    fn a_missing_grant_tells_the_user_every_action_kind_is_refused() {
        let mut definition = definition_with_grant();
        definition.unattended = None;
        let text = grant_text(&definition);
        assert!(text.contains("shell"), "{text}");
        assert!(text.contains("native"), "{text}");
        assert!(text.contains("agent"), "{text}");
        assert!(text.contains("authoriz"), "{text}");
        assert!(grant_json(&definition).is_null());
    }

    #[test]
    fn a_current_grant_is_not_marked_for_reauthorization() {
        let definition = definition_with_grant();
        assert_eq!(
            grant_json(&definition)["requires_regrant"],
            serde_json::json!(false)
        );
        assert!(!grant_text(&definition).contains("re-authorize"));
    }

    #[test]
    fn an_old_grant_version_is_reported_as_requiring_reauthorization() {
        let mut definition = definition_with_grant();
        definition.unattended.as_mut().expect("grant").version = 2;
        assert_eq!(
            grant_json(&definition)["requires_regrant"],
            serde_json::json!(true)
        );
        let text = grant_text(&definition);
        assert!(text.contains("re-authorize"), "{text}");
        assert!(
            definition
                .unattended_authorization(ActionKind::Shell, true)
                .is_err(),
            "the old grant must stay fail-closed"
        );
    }
}
