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

use anyhow::Context as _;
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use leveler_client_protocol::{ModelRef, PermissionProfile};
use leveler_local_transport::LocalRuntimeService;
use leveler_project::Layout;
use leveler_runtime_host::{
    DetachedRuntimeLaunch, NonInteractiveHandoffUi, ensure_default_runtime,
};
use leveler_schedule::{
    DispatchSummary, Dispatcher, MisfirePolicy, NewScheduleDefinition, RuntimeConnector,
    ScheduleStore, ScheduleTrigger, WakeSource,
    wakeup::{self, WAKE_INTERVAL, WakeupSpec, WakeupStatus},
};

use crate::cli::{RunMode, ScheduleCommand, ScheduleMisfire, ScheduleSource};

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
            let trigger = match (at, every) {
                (Some(at), None) => ScheduleTrigger::Once {
                    at: parse_time(&at)?,
                },
                (None, Some(every)) => ScheduleTrigger::Every {
                    anchor: anchor
                        .as_deref()
                        .map(parse_time)
                        .transpose()?
                        .unwrap_or_else(Utc::now),
                    every: Duration::from_secs(every),
                },
                (None, None) => {
                    anyhow::bail!("a schedule needs --at <RFC3339> or --every <SECONDS>")
                }
                (Some(_), Some(_)) => unreachable!("clap conflicts --at with --every"),
            };
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
                    command,
                    trigger,
                    mode: schedule_mode(mode),
                    model,
                    timeout: Duration::from_secs(timeout),
                    misfire: misfire_policy(misfire),
                    misfire_grace: Duration::from_secs(misfire_grace),
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
                    "misfire": definition.misfire.as_str(),
                    "misfire_grace_secs": definition.misfire_grace.as_secs(),
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
                            "misfire": definition.misfire.as_str(),
                            "misfire_grace_secs": definition.misfire_grace.as_secs(),
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
                        if definition.enabled {
                            "enabled"
                        } else {
                            "disabled"
                        },
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
            "missed": summary.missed,
            "claimed": summary.claimed,
            "succeeded": summary.succeeded,
            "failed": summary.failed,
            "unknown": summary.unknown,
        }));
    }
    println!(
        "recovered={} missed={} claimed={} succeeded={} failed={} unknown={}",
        summary.recovered,
        summary.missed,
        summary.claimed,
        summary.succeeded,
        summary.failed,
        summary.unknown
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
    for key in [
        "PATH",
        "HOME",
        "TMPDIR",
        "LEVELER_DAEMON_IDLE_TIMEOUT_SECS",
    ] {
        if let Some(value) = snapshot.var_os(key).and_then(|value| value.to_str().map(str::to_owned))
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
        "missed": wake.missed,
        "claimed": wake.claimed,
        "succeeded": wake.succeeded,
        "failed": wake.failed,
        "unknown": wake.unknown,
        "error": wake.error,
    })
}

fn wake_line(wake: &leveler_schedule::WakeRecord) -> String {
    format!(
        "#{} {} started={} finished={} recovered={} missed={} claimed={} succeeded={} failed={} unknown={}{}",
        wake.id,
        wake.source.as_str(),
        wake.started_at.to_rfc3339(),
        wake.finished_at
            .map(|finished| finished.to_rfc3339())
            .unwrap_or_else(|| "unfinished".into()),
        wake.recovered,
        wake.missed,
        wake.claimed,
        wake.succeeded,
        wake.failed,
        wake.unknown,
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
                "the OS record exists but is not loaded; run `leveler schedule enable`"
                    .into(),
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
                    push(
                        "arguments",
                        CheckSeverity::Ok,
                        spec.arguments.join(" "),
                    );
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
