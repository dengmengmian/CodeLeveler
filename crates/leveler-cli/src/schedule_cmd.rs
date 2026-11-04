//! `leveler schedule`: the user-global scheduled-action surface.
//!
//! Both drivers are the same dispatcher: `dispatch` is the short-lived path an
//! OS timer can invoke, and `serve` is a lightweight resident loop. Neither
//! owns a runtime, a session DB or an agent loop; both connect to the
//! workspace's ordinary runtime as a local client and disconnect after each
//! run, so the daemon's existing idle eviction stays in force.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::Context as _;
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use leveler_client_protocol::{ModelRef, PermissionProfile};
use leveler_local_transport::LocalRuntimeService;
use leveler_project::Layout;
use leveler_runtime_host::{DetachedRuntimeLaunch, NonInteractiveHandoffUi, ensure_default_runtime};
use leveler_schedule::{
    DispatchSummary, Dispatcher, NewScheduleDefinition, RuntimeConnector, ScheduleStore,
    ScheduleTrigger,
};

use crate::cli::{RunMode, ScheduleCommand};

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
            at,
            every,
            anchor,
            mode,
            model,
            timeout,
            json,
        } => {
            let workspace = match workspace {
                Some(workspace) => workspace,
                None => layout
                    .primary_workspace()
                    .map(Path::to_path_buf)
                    .context("this command has no workspace; pass --workspace")?,
            };
            let trigger = match (at, every) {
                (Some(at), None) => ScheduleTrigger::Once {
                    at: parse_time(&at)?,
                },
                (None, Some(every)) => ScheduleTrigger::Every {
                    anchor: anchor.as_deref().map(parse_time).transpose()?.unwrap_or_else(Utc::now),
                    every: Duration::from_secs(every),
                },
                (None, None) => {
                    anyhow::bail!("a schedule needs --at <RFC3339> or --every <SECONDS>")
                }
                (Some(_), Some(_)) => unreachable!("clap conflicts --at with --every"),
            };
            let model = match model {
                Some(reference) => Some(ModelRef::parse(&reference).with_context(|| {
                    format!("`{reference}` is not a valid model reference")
                })?),
                None => None,
            };
            let definition = store
                .create_definition(NewScheduleDefinition {
                    name,
                    workspace,
                    command,
                    trigger,
                    mode: schedule_mode(mode),
                    model,
                    timeout: Duration::from_secs(timeout),
                })
                .await?;
            let next = store
                .occurrences(&definition.id, 1)
                .await?
                .into_iter()
                .next()
                .map(|occurrence| occurrence.scheduled_for.to_rfc3339());
            if json {
                print_json(&serde_json::json!({
                    "id": definition.id,
                    "name": definition.name,
                    "workspace": definition.workspace,
                    "command": definition.command,
                    "trigger": trigger_json(&definition.trigger),
                    "mode": definition.mode.as_str(),
                    "model": definition.model.as_ref().map(|model| model.to_string()),
                    "timeout_secs": definition.timeout.as_secs(),
                    "next_occurrence": next,
                }))?;
            } else {
                println!("added schedule {}", definition.id);
                println!("  name:      {}", definition.name);
                println!("  workspace: {}", definition.workspace.display());
                println!("  trigger:   {}", trigger_text(&definition.trigger));
                println!("  next:      {}", next.unwrap_or_else(|| "none".into()));
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
                            "command": definition.command,
                            "trigger": trigger_json(&definition.trigger),
                            "enabled": definition.enabled,
                            "session_id": definition.session_id,
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
                        "{}  {}  {}  {}  session={}",
                        definition.id,
                        definition.name,
                        trigger_text(&definition.trigger),
                        if definition.enabled { "enabled" } else { "disabled" },
                        definition
                            .session_id
                            .as_ref()
                            .map(|session| session.as_str())
                            .unwrap_or("-"),
                    );
                    println!("  workspace: {}", definition.workspace.display());
                }
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
                    println!(
                        "{}  {}  {}  exit={}  started={}",
                        run.id,
                        run.definition_id,
                        run.state.as_str(),
                        run.exit_code
                            .map(|code| code.to_string())
                            .unwrap_or_else(|| "-".into()),
                        run.started_at.to_rfc3339(),
                    );
                    if let Some(error) = run.error {
                        println!("  error: {error}");
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
        ScheduleCommand::Dispatch { max, json } => {
            let summary = dispatcher(config_dir, store).dispatch_due(max).await?;
            print_summary(&summary, json)?;
        }
        ScheduleCommand::Serve { max, poll_secs } => {
            serve(config_dir, store, max, poll_secs).await?;
        }
    }
    Ok(())
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
        let summary = dispatcher.dispatch_due(max).await?;
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
        config_dir,
        launch: DetachedRuntimeLaunch {
            executable: std::env::current_exe().unwrap_or_else(|_| PathBuf::from("leveler")),
            ready_prefix: "leveler-schedule-ready".into(),
        },
    };
    Dispatcher::new(store, Arc::new(connector)).with_lease(DISPATCH_LEASE)
}

struct DaemonConnector {
    config_dir: Option<PathBuf>,
    launch: DetachedRuntimeLaunch,
}

#[async_trait]
impl RuntimeConnector for DaemonConnector {
    async fn connect(&self, workspace: &Path) -> Result<Arc<dyn LocalRuntimeService>, String> {
        let layout = Layout::resolve(workspace.to_path_buf(), self.config_dir.clone());
        let client = ensure_default_runtime(
            &layout,
            &self.launch,
            NonInteractiveHandoffUi::new(),
        )
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

fn trigger_text(trigger: &ScheduleTrigger) -> String {
    match trigger {
        ScheduleTrigger::Once { at } => format!("once at {}", at.to_rfc3339()),
        ScheduleTrigger::Every { anchor, every } => {
            format!("every {}s from {}", every.as_secs(), anchor.to_rfc3339())
        }
    }
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
    }
}

fn print_summary(summary: &DispatchSummary, json: bool) -> anyhow::Result<()> {
    if json {
        return print_json(&serde_json::json!({
            "recovered": summary.recovered,
            "claimed": summary.claimed,
            "succeeded": summary.succeeded,
            "failed": summary.failed,
            "unknown": summary.unknown,
        }));
    }
    println!(
        "recovered={} claimed={} succeeded={} failed={} unknown={}",
        summary.recovered, summary.claimed, summary.succeeded, summary.failed, summary.unknown
    );
    Ok(())
}

fn print_json(value: &serde_json::Value) -> anyhow::Result<()> {
    println!("{}", serde_json::to_string_pretty(value)?);
    Ok(())
}
