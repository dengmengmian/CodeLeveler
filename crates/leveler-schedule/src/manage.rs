//! Structured schedule management shared by every product surface.
//!
//! The store remains the only authority for definitions, occurrences, runs and
//! grants. This module resolves a wire [`ScheduleRequest`] against that
//! authority and projects the result into the protocol's view types; it never
//! keeps a second list, parses human CLI output, or runs the scheduler itself.
//! A `RunNow` request only creates the explicit slot the existing dispatcher
//! claims, so no entry point becomes a second execution authority.
//!
//! Timezone rules are real IANA zones: a calendar trigger that omits one uses
//! the host's current wall-clock zone, never a UTC assumption.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, SecondsFormat, Utc};
use leveler_client_protocol::schedule::{
    ScheduleActionWire, ScheduleCheck, ScheduleDraft, ScheduleFilter, ScheduleGrantView,
    SchedulePatch, ScheduleRequest, ScheduleResponse, ScheduleRunView, ScheduleStatusView,
    ScheduleTriggerWire, ScheduleUsageView, ScheduleView, ScheduleWakeView, ScheduleWakeupView,
};
use leveler_client_protocol::{ModelRef, PermissionProfile};

use crate::{
    DefinitionUpdate, LocalTime, MisfirePolicy, NewScheduleDefinition, Occurrence, OverlapPolicy,
    RunState, ScheduleAction, ScheduleDefinition, ScheduleError, ScheduleId, ScheduleRun,
    ScheduleStore, ScheduleTrigger, SessionPolicy, UnattendedRequest, WeekdaySet, host_timezone,
    parse_cron, parse_timezone, wakeup,
};

/// Longest identifier accepted from a client. Generated ids are UUIDs; the
/// bound keeps a malformed request from reaching storage as an unbounded key.
const MAX_ID_BYTES: usize = 128;
const DEFAULT_RUN_LIMIT: u32 = 50;
const MAX_RUN_LIMIT: u32 = 500;
const DEFAULT_TIMEOUT_SECS: u64 = 600;
const DEFAULT_MISFIRE_GRACE_SECS: u64 = 300;

/// The management surface over one schedule database.
#[derive(Clone)]
pub struct ScheduleManager {
    store: Arc<ScheduleStore>,
    home: PathBuf,
    /// The config directory the invoking process actually uses. The OS job
    /// does not inherit a shell, so it must travel with the registration when
    /// one was named. `None` keeps the dispatcher's per-workspace default.
    config_dir: Option<PathBuf>,
}

impl ScheduleManager {
    /// Open the user-global schedule database for `home`.
    pub async fn open(
        home: &leveler_core::LevelerHome,
        config_dir: Option<PathBuf>,
    ) -> Result<Self, ScheduleError> {
        let store = ScheduleStore::open(&home.schedules_database()).await?;
        Ok(Self::new(
            Arc::new(store),
            home.root().to_path_buf(),
            config_dir,
        ))
    }

    pub fn new(store: Arc<ScheduleStore>, home: PathBuf, config_dir: Option<PathBuf>) -> Self {
        Self {
            store,
            home,
            config_dir,
        }
    }

    pub fn store(&self) -> &Arc<ScheduleStore> {
        &self.store
    }

    /// Resolve one request. Domain failures become a structured `Error`
    /// response so every client renders the same fact instead of inventing a
    /// local fallback.
    pub async fn handle(&self, request: ScheduleRequest) -> ScheduleResponse {
        match self.dispatch(request).await {
            Ok(response) => response,
            Err(error) => ScheduleResponse::Error {
                message: error.to_string(),
            },
        }
    }

    async fn dispatch(&self, request: ScheduleRequest) -> Result<ScheduleResponse, ScheduleError> {
        match request {
            ScheduleRequest::List { filter } => self.list(filter).await,
            ScheduleRequest::Show { id } => self.show(&id).await,
            ScheduleRequest::Create { draft } => self.create(draft).await,
            ScheduleRequest::Update {
                id,
                expected_revision,
                patch,
            } => self.update(&id, expected_revision.as_deref(), patch).await,
            ScheduleRequest::Pause { id } => self.set_enabled(&id, false).await,
            ScheduleRequest::Resume { id } => self.set_enabled(&id, true).await,
            ScheduleRequest::RunNow { id } => self.run_now(&id).await,
            ScheduleRequest::Cancel { id } => self.cancel(&id).await,
            ScheduleRequest::Remove { id } => self.remove(&id).await,
            ScheduleRequest::Runs { definition, limit } => {
                self.runs(definition.as_deref(), limit).await
            }
            ScheduleRequest::Grant {
                id,
                allow_external_side_effects,
            } => self.grant(&id, allow_external_side_effects).await,
            ScheduleRequest::RevokeGrant { id } => self.revoke_grant(&id).await,
            ScheduleRequest::Status => self.status().await,
            ScheduleRequest::WakeupEnable => self.wakeup_enable().await,
            ScheduleRequest::WakeupDisable => self.wakeup_disable().await,
            ScheduleRequest::WakeupDoctor => self.wakeup_doctor().await,
        }
    }

    async fn list(
        &self,
        filter: Option<ScheduleFilter>,
    ) -> Result<ScheduleResponse, ScheduleError> {
        let definitions = self.store.definitions().await?;
        let latest = self.store.latest_run_per_definition().await?;
        let next = self.store.next_pending_occurrences().await?;
        let active = self.store.active_run_counts().await?;
        let mut schedules = Vec::with_capacity(definitions.len());
        for definition in &definitions {
            let run = latest.iter().find(|run| run.definition_id == definition.id);
            let occurrence = next
                .iter()
                .find(|occurrence| occurrence.definition_id == definition.id);
            let running = active.get(&definition.id).copied().unwrap_or(0) > 0;
            schedules.push(project(definition, run, running, occurrence)?);
        }
        if let Some(filter) = filter {
            schedules.retain(|schedule| matches_filter(filter, schedule));
        }
        Ok(ScheduleResponse::List { schedules })
    }

    async fn show(&self, id: &str) -> Result<ScheduleResponse, ScheduleError> {
        let id = parse_id(id)?;
        let definition = self.load(&id).await?;
        Ok(ScheduleResponse::Show {
            schedule: self.view(&definition).await?,
        })
    }

    async fn create(&self, draft: ScheduleDraft) -> Result<ScheduleResponse, ScheduleError> {
        if draft.name.trim().is_empty() {
            return Err(ScheduleError::Invalid("the schedule name is empty".into()));
        }
        if draft.workspace.trim().is_empty() {
            return Err(ScheduleError::Invalid(
                "the schedule workspace is empty; name the directory whose runtime owns the \
                 action"
                    .into(),
            ));
        }
        if draft.workspace.len() > 4096 {
            return Err(ScheduleError::Invalid(
                "the schedule workspace path is too long".into(),
            ));
        }
        if draft.allow_external_side_effects && !draft.grant_unattended {
            return Err(ScheduleError::Invalid(
                "external side effects may only be authorized together with an unattended grant"
                    .into(),
            ));
        }
        let model = match draft.model.as_deref() {
            Some(raw) => parse_model(raw)?,
            None => None,
        };
        let definition = self
            .store
            .create_definition(NewScheduleDefinition {
                name: draft.name,
                workspace: PathBuf::from(draft.workspace),
                action: action_from_wire(draft.action)?,
                trigger: trigger_from_wire(draft.trigger)?,
                session_policy: draft
                    .session_policy
                    .as_deref()
                    .map(parse_session_policy)
                    .transpose()?
                    .unwrap_or(SessionPolicy::Continue),
                overlap: draft
                    .overlap
                    .as_deref()
                    .map(parse_overlap)
                    .transpose()?
                    .unwrap_or(OverlapPolicy::SkipIfRunning),
                mode: draft
                    .mode
                    .as_deref()
                    .map(parse_mode)
                    .transpose()?
                    .unwrap_or(PermissionProfile::Assisted),
                model,
                timeout: Duration::from_secs(draft.timeout_secs.unwrap_or(DEFAULT_TIMEOUT_SECS)),
                misfire: draft
                    .misfire
                    .as_deref()
                    .map(parse_misfire)
                    .transpose()?
                    .unwrap_or(MisfirePolicy::Skip),
                misfire_grace: Duration::from_secs(
                    draft
                        .misfire_grace_secs
                        .unwrap_or(DEFAULT_MISFIRE_GRACE_SECS),
                ),
                unattended: draft.grant_unattended.then_some(UnattendedRequest {
                    allow_external_side_effects: draft.allow_external_side_effects,
                }),
            })
            .await?;
        Ok(ScheduleResponse::Changed {
            schedule: Some(self.view(&definition).await?),
            message: "schedule created".into(),
        })
    }

    async fn update(
        &self,
        id: &str,
        expected_revision: Option<&str>,
        patch: SchedulePatch,
    ) -> Result<ScheduleResponse, ScheduleError> {
        let id = parse_id(id)?;
        let existing = self.load(&id).await?;
        let expected = match expected_revision {
            Some(raw) => Some(parse_revision(raw)?),
            None => None,
        };
        let action = match patch.action {
            Some(action) => action_from_wire(action)?,
            None => existing.action.clone(),
        };
        let trigger = match patch.trigger {
            Some(trigger) => trigger_from_wire(trigger)?,
            None => existing.trigger.clone(),
        };
        let model = match patch.model.as_deref() {
            Some("") => None,
            Some(raw) => parse_model(raw)?,
            None => existing.model.clone(),
        };
        let update = DefinitionUpdate {
            name: patch.name.unwrap_or_else(|| existing.name.clone()),
            workspace: patch
                .workspace
                .map(PathBuf::from)
                .unwrap_or_else(|| existing.workspace.clone()),
            action,
            trigger,
            session_policy: patch
                .session_policy
                .as_deref()
                .map(parse_session_policy)
                .transpose()?
                .unwrap_or(existing.session_policy),
            overlap: patch
                .overlap
                .as_deref()
                .map(parse_overlap)
                .transpose()?
                .unwrap_or(existing.overlap),
            mode: patch
                .mode
                .as_deref()
                .map(parse_mode)
                .transpose()?
                .unwrap_or(existing.mode),
            model,
            timeout: Duration::from_secs(patch.timeout_secs.unwrap_or(existing.timeout.as_secs())),
            misfire: patch
                .misfire
                .as_deref()
                .map(parse_misfire)
                .transpose()?
                .unwrap_or(existing.misfire),
            misfire_grace: Duration::from_secs(
                patch
                    .misfire_grace_secs
                    .unwrap_or(existing.misfire_grace.as_secs()),
            ),
            // A grant is kept only while it still covers the new declaration;
            // the store drops it otherwise rather than widening it.
            grant: None,
        };
        let definition = self.store.update_definition(&id, update, expected).await?;
        Ok(ScheduleResponse::Changed {
            schedule: Some(self.view(&definition).await?),
            message: "schedule updated".into(),
        })
    }

    async fn set_enabled(
        &self,
        id: &str,
        enabled: bool,
    ) -> Result<ScheduleResponse, ScheduleError> {
        let id = parse_id(id)?;
        let definition = self.load(&id).await?;
        if definition.enabled != enabled {
            self.store.set_enabled(&id, enabled).await?;
        }
        let definition = self.load(&id).await?;
        Ok(ScheduleResponse::Changed {
            schedule: Some(self.view(&definition).await?),
            message: if enabled {
                "schedule resumed; future slots are claimable again".into()
            } else {
                "schedule paused; an active run is not stopped by pausing".into()
            },
        })
    }

    async fn run_now(&self, id: &str) -> Result<ScheduleResponse, ScheduleError> {
        let id = parse_id(id)?;
        let definition = self.load(&id).await?;
        let occurrence = self.store.create_run_now(&definition).await?;
        Ok(ScheduleResponse::Changed {
            schedule: Some(self.view(&definition).await?),
            message: format!(
                "run-now slot {} queued at {}; the existing dispatcher executes it",
                occurrence.id,
                render_time(occurrence.scheduled_for)
            ),
        })
    }

    async fn cancel(&self, id: &str) -> Result<ScheduleResponse, ScheduleError> {
        let id = parse_id(id)?;
        let definition = self.load(&id).await?;
        let runs = self.store.request_cancel_definition(&id).await?;
        Ok(ScheduleResponse::Changed {
            schedule: Some(self.view(&definition).await?),
            message: if runs.is_empty() {
                "the schedule has no active run".into()
            } else {
                format!(
                    "cancellation requested for {} active run(s); the terminal fact is recorded \
                     only after the runtime reports it",
                    runs.len()
                )
            },
        })
    }

    async fn remove(&self, id: &str) -> Result<ScheduleResponse, ScheduleError> {
        let id = parse_id(id)?;
        self.load(&id).await?;
        if !self.store.remove_definition(&id).await? {
            return Err(ScheduleError::NotFound(id));
        }
        Ok(ScheduleResponse::Removed { id: id.to_string() })
    }

    async fn runs(
        &self,
        definition: Option<&str>,
        limit: Option<u32>,
    ) -> Result<ScheduleResponse, ScheduleError> {
        let definition = definition.map(parse_id).transpose()?;
        if let Some(id) = &definition {
            self.load(id).await?;
        }
        let limit = limit.unwrap_or(DEFAULT_RUN_LIMIT).clamp(1, MAX_RUN_LIMIT) as usize;
        let runs = self.store.runs(definition.as_ref(), limit).await?;
        Ok(ScheduleResponse::Runs {
            runs: runs.iter().map(run_view).collect(),
        })
    }

    async fn grant(
        &self,
        id: &str,
        allow_external_side_effects: bool,
    ) -> Result<ScheduleResponse, ScheduleError> {
        let id = parse_id(id)?;
        let definition = self.load(&id).await?;
        let update = DefinitionUpdate {
            name: definition.name.clone(),
            workspace: definition.workspace.clone(),
            action: definition.action.clone(),
            trigger: definition.trigger.clone(),
            session_policy: definition.session_policy,
            overlap: definition.overlap,
            mode: definition.mode,
            model: definition.model.clone(),
            timeout: definition.timeout,
            misfire: definition.misfire,
            misfire_grace: definition.misfire_grace,
            grant: Some(UnattendedRequest {
                allow_external_side_effects,
            }),
        };
        let definition = self
            .store
            .update_definition(&id, update, Some(definition.updated_at))
            .await?;
        Ok(ScheduleResponse::Changed {
            schedule: Some(self.view(&definition).await?),
            message: if allow_external_side_effects {
                "unattended grant issued, including actions with external side effects".into()
            } else {
                "unattended grant issued for actions without external side effects".into()
            },
        })
    }

    async fn revoke_grant(&self, id: &str) -> Result<ScheduleResponse, ScheduleError> {
        let id = parse_id(id)?;
        let revoked = self.store.revoke_unattended(&id).await?;
        let definition = self.load(&id).await?;
        Ok(ScheduleResponse::Changed {
            schedule: Some(self.view(&definition).await?),
            message: if revoked {
                "unattended grant revoked; the next unattended run needs a new authorization".into()
            } else {
                "the schedule had no persisted unattended grant".into()
            },
        })
    }

    async fn status(&self) -> Result<ScheduleResponse, ScheduleError> {
        let definitions = self.store.definitions().await?;
        let active = self.store.active_run_counts().await?;
        let next = self.store.next_due().await?;
        let overdue = self.store.overdue_pending(leveler_core::now()).await?;
        let last_os_wake = self.store.last_os_wake().await?;
        let enabled = definitions.iter().filter(|item| item.enabled).count();
        Ok(ScheduleResponse::Status {
            status: ScheduleStatusView {
                wakeup: self.wakeup_view().await,
                definitions: definitions.len(),
                enabled,
                paused: definitions.len() - enabled,
                running: active.values().sum(),
                next_occurrence: next.map(render_time),
                overdue_pending: overdue,
                last_os_wake: last_os_wake.as_ref().map(wake_view),
            },
        })
    }

    async fn wakeup_enable(&self) -> Result<ScheduleResponse, ScheduleError> {
        let spec = self.wakeup_spec()?;
        wakeup::enable(&spec)
            .await
            .map_err(|error| ScheduleError::Invalid(error.to_string()))?;
        self.status().await
    }

    async fn wakeup_disable(&self) -> Result<ScheduleResponse, ScheduleError> {
        wakeup::disable(&self.home)
            .await
            .map_err(|error| ScheduleError::Invalid(error.to_string()))?;
        self.status().await
    }

    async fn wakeup_doctor(&self) -> Result<ScheduleResponse, ScheduleError> {
        let status = wakeup::status(&self.home)
            .await
            .map_err(|error| ScheduleError::Invalid(error.to_string()))?;
        let spec = self.wakeup_spec().ok();
        let definitions = self.store.definitions().await?;
        let overdue = self.store.overdue_pending(leveler_core::now()).await?;
        let last_os_wake = self.store.last_os_wake().await?;
        let (healthy, checks) = doctor_checks(
            &status,
            spec.as_ref(),
            definitions.len(),
            overdue,
            last_os_wake.as_ref(),
        );
        Ok(ScheduleResponse::Doctor { healthy, checks })
    }

    async fn load(&self, id: &ScheduleId) -> Result<ScheduleDefinition, ScheduleError> {
        self.store
            .definition(id)
            .await?
            .ok_or_else(|| ScheduleError::NotFound(id.clone()))
    }

    async fn view(&self, definition: &ScheduleDefinition) -> Result<ScheduleView, ScheduleError> {
        let run = self
            .store
            .runs(Some(&definition.id), 1)
            .await?
            .into_iter()
            .next();
        let active = self.store.active_run_count(&definition.id).await?;
        let occurrence = self.store.next_pending_occurrence(&definition.id).await?;
        project(definition, run.as_ref(), active > 0, occurrence.as_ref())
    }

    fn wakeup_spec(&self) -> Result<wakeup::WakeupSpec, ScheduleError> {
        let executable = wakeup::preferred_executable()
            .map_err(|error| ScheduleError::Invalid(error.to_string()))?;
        Ok(wakeup::WakeupSpec::standard(
            self.home.clone(),
            executable,
            self.config_dir.as_deref(),
            leveler_core::environment(),
        ))
    }

    async fn wakeup_view(&self) -> ScheduleWakeupView {
        match wakeup::status(&self.home).await {
            Ok(status) => wakeup_view(&status),
            Err(error) => ScheduleWakeupView {
                backend: wakeup::backend_name().to_string(),
                supported: wakeup::is_supported(),
                label: wakeup::label_for(&self.home),
                registered: false,
                loaded: false,
                registration_path: None,
                last_exit_code: None,
                run_count: None,
                detail: Some(format!("could not read the OS registration: {error}")),
                interval_secs: None,
            },
        }
    }
}

fn project(
    definition: &ScheduleDefinition,
    run: Option<&ScheduleRun>,
    running: bool,
    next: Option<&Occurrence>,
) -> Result<ScheduleView, ScheduleError> {
    let grant = grant_view(definition);
    let attention_reason = match run.map(|run| run.state) {
        Some(RunState::WaitingApproval) => {
            Some("the last run stopped because an unattended approval is missing".to_string())
        }
        Some(RunState::Blocked) => Some(
            run.and_then(|run| run.error.clone())
                .unwrap_or_else(|| "the last run was blocked".into()),
        ),
        Some(RunState::Unknown) => Some("the last run's result could not be confirmed".to_string()),
        Some(RunState::Failed) => Some(
            run.and_then(|run| run.error.clone())
                .unwrap_or_else(|| "the last run failed".into()),
        ),
        _ if grant.requires_regrant && !grant.granted => {
            Some("unattended execution is not authorized; the next run needs a grant".to_string())
        }
        _ if grant.requires_regrant => {
            Some("the stored unattended grant no longer matches this schedule".to_string())
        }
        _ => None,
    };
    Ok(ScheduleView {
        id: definition.id.to_string(),
        name: definition.name.clone(),
        description: describe(&definition.action),
        action_kind: definition.action.kind().as_str().to_string(),
        action: action_wire(&definition.action)?,
        workspace: definition.workspace.display().to_string(),
        trigger: trigger_wire(&definition.trigger),
        trigger_text: definition.trigger.describe(),
        timezone: definition
            .trigger
            .timezone()
            .map(|timezone| timezone.to_string()),
        enabled: definition.enabled,
        running,
        run_state: run.map(|run| run.state.as_str().to_string()),
        next_run_at: next.map(|occurrence| render_time(occurrence.scheduled_for)),
        last_run: run.map(run_view),
        needs_attention: attention_reason.is_some(),
        attention_reason,
        session_policy: definition.session_policy.as_str().to_string(),
        overlap: definition.overlap.as_str().to_string(),
        mode: definition.mode.as_str().to_string(),
        model: definition.model.as_ref().map(ModelRef::to_string),
        timeout_secs: definition.timeout.as_secs(),
        misfire: definition.misfire.as_str().to_string(),
        misfire_grace_secs: definition.misfire_grace.as_secs(),
        session_id: definition.session_id.as_ref().map(ToString::to_string),
        grant,
        created_at: render_time(definition.created_at),
        revision: render_time(definition.updated_at),
    })
}

fn matches_filter(filter: ScheduleFilter, schedule: &ScheduleView) -> bool {
    match filter {
        ScheduleFilter::All => true,
        ScheduleFilter::Enabled => schedule.enabled,
        ScheduleFilter::Paused => !schedule.enabled,
        ScheduleFilter::Running => schedule.running,
        ScheduleFilter::NeedsAttention => schedule.needs_attention,
        ScheduleFilter::RecentFailure => matches!(
            schedule.run_state.as_deref(),
            Some("failed" | "unknown" | "blocked")
        ),
    }
}

fn describe(action: &ScheduleAction) -> String {
    match action {
        ScheduleAction::Shell { command } => command.clone(),
        ScheduleAction::Native { action, .. } => format!("native action `{action}`"),
        ScheduleAction::Agent { goal } => goal.clone(),
    }
}

fn grant_view(definition: &ScheduleDefinition) -> ScheduleGrantView {
    let Some(grant) = definition.unattended.as_ref() else {
        return ScheduleGrantView {
            granted: false,
            external_side_effects: false,
            granted_at: None,
            // A missing grant is a real missing authorization, not a neutral
            // "no grant": every permission profile needs one for unattended
            // execution, and the surface must ask the user to issue it.
            requires_regrant: true,
        };
    };
    // The static identity check ignores the observed side-effect
    // classification: whether a particular run may touch the network is only
    // known when it runs, and the grant names the scope the user accepted.
    let valid = grant
        .covers(definition, definition.action.kind(), false)
        .is_ok();
    ScheduleGrantView {
        granted: true,
        external_side_effects: grant.external_side_effects,
        granted_at: Some(render_time(grant.granted_at)),
        requires_regrant: !valid,
    }
}

fn run_view(run: &ScheduleRun) -> ScheduleRunView {
    ScheduleRunView {
        id: run.id.to_string(),
        state: run.state.as_str().to_string(),
        scheduled_for: render_time(run.scheduled_for),
        started_at: render_time(run.started_at),
        finished_at: run.finished_at.map(render_time),
        session_id: run.session_id.as_ref().map(ToString::to_string),
        execution_id: run.execution_id.clone(),
        exit_code: run.exit_code,
        error: run.error.clone(),
        provider: run.provider.clone(),
        native_outcome: run.native_outcome.clone(),
        task_outcome: run.task_outcome.clone(),
        capability_state: run.capability_state.clone(),
        permission_state: run.permission_state.clone(),
        usage: usage_view(run),
        cancel_requested_at: run.cancel_requested_at.map(render_time),
    }
}

fn usage_view(run: &ScheduleRun) -> Option<ScheduleUsageView> {
    // The usage columns are written together from one observed delta. A run
    // with no observed baseline has none of them, and no figure is invented.
    let requests = run.usage_requests?;
    Some(ScheduleUsageView {
        requests,
        input_tokens: run.input_tokens?,
        cached_input_tokens: run.cached_input_tokens?,
        output_tokens: run.output_tokens?,
        reasoning_tokens: run.reasoning_tokens?,
        cost_usd_micros: run.cost_usd_micros?,
        rows_without_cost: run.usage_rows_without_cost?,
    })
}

fn wake_view(wake: &crate::WakeRecord) -> ScheduleWakeView {
    ScheduleWakeView {
        id: wake.id,
        source: wake.source.as_str().to_string(),
        started_at: render_time(wake.started_at),
        finished_at: wake.finished_at.map(render_time),
        claimed: wake.claimed,
        succeeded: wake.succeeded,
        failed: wake.failed,
        unknown: wake.unknown,
        skipped: wake.skipped,
        missed: wake.missed,
        error: wake.error.clone(),
    }
}

fn wakeup_view(status: &wakeup::WakeupStatus) -> ScheduleWakeupView {
    ScheduleWakeupView {
        backend: status.backend.to_string(),
        supported: status.supported,
        label: status.label.clone(),
        registered: status.registered,
        loaded: status.loaded,
        registration_path: status
            .registration_path
            .as_ref()
            .map(|path| path.display().to_string()),
        last_exit_code: status.last_exit_code,
        run_count: status.run_count,
        detail: status.detail.clone(),
        interval_secs: status
            .registration
            .as_ref()
            .and_then(|registration| registration.interval)
            .map(|interval| interval.as_secs()),
    }
}

/// Read-only consistency checks shared by every entry point. A missing
/// opt-in registration is a warning when schedules exist; a record that
/// disagrees with this build's deterministic command is a failure, because the
/// next wake would run a different executable or configuration than the user
/// enabled.
pub fn doctor_checks(
    status: &wakeup::WakeupStatus,
    spec: Option<&wakeup::WakeupSpec>,
    definitions: usize,
    overdue: usize,
    last_os_wake: Option<&crate::WakeRecord>,
) -> (bool, Vec<ScheduleCheck>) {
    let mut checks = Vec::new();
    let mut push = |name: &str, severity: &str, detail: String| {
        checks.push(ScheduleCheck {
            name: name.to_string(),
            severity: severity.to_string(),
            detail,
        })
    };

    if status.supported {
        push("platform", "ok", format!("{} is available", status.backend));
    } else {
        push(
            "platform",
            "fail",
            format!("OS wakeup is not supported by backend `{}`", status.backend),
        );
    }

    if status.supported {
        if definitions == 0 {
            if status.registered {
                push(
                    "registration",
                    "warn",
                    "an OS wakeup is registered but no schedule is defined".into(),
                );
            } else {
                push(
                    "registration",
                    "ok",
                    "no schedule is defined, so no OS wakeup is required".into(),
                );
            }
        } else if !status.registered {
            push(
                "registration",
                "warn",
                format!(
                    "{definitions} schedule definition(s) exist but no OS wakeup is \
                     registered; they run only while a dispatcher is started. Run \
                     `leveler schedule enable` for unattended wakeups"
                ),
            );
        } else if !status.loaded {
            push(
                "registration",
                "fail",
                "the OS record exists but is not loaded; run `leveler schedule enable`".into(),
            );
        } else {
            push(
                "registration",
                "ok",
                format!("{} is registered and loaded", status.label),
            );
        }

        match (spec, status.registration.as_ref()) {
            (Some(spec), Some(registration)) => {
                if registration.executable == spec.executable {
                    push("executable", "ok", spec.executable.display().to_string());
                } else {
                    push(
                        "executable",
                        "fail",
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
                        "fail",
                        format!(
                            "the registered executable `{}` no longer exists",
                            registration.executable.display()
                        ),
                    );
                }
                if registration.arguments == spec.arguments {
                    push("arguments", "ok", spec.arguments.join(" "));
                } else {
                    push(
                        "arguments",
                        "fail",
                        format!(
                            "registered `{}` but this build would use `{}`",
                            registration.arguments.join(" "),
                            spec.arguments.join(" ")
                        ),
                    );
                }
                if registration.interval == Some(spec.interval) {
                    push("interval", "ok", format!("{}s", spec.interval.as_secs()));
                } else {
                    push(
                        "interval",
                        "fail",
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
                        "ok",
                        format!("{} variable(s) match", spec.environment.len()),
                    );
                } else {
                    push(
                        "environment",
                        "fail",
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
                "fail",
                "the OS record exists but its contents could not be read".into(),
            ),
            (None, _) => push(
                "executable",
                "fail",
                "the leveler executable could not be resolved for this registration".into(),
            ),
            (Some(_), None) => {}
        }
    }

    if overdue == 0 {
        push(
            "overdue",
            "ok",
            "no due slot is waiting for a dispatcher".into(),
        );
    } else {
        push(
            "overdue",
            "warn",
            format!(
                "{overdue} due slot(s) are pending; the next wake will dispatch them or \
                 apply the misfire policy"
            ),
        );
    }

    match last_os_wake {
        Some(wake) if wake.finished_at.is_none() => push(
            "last_os_wake",
            "warn",
            format!(
                "wake #{} started {} and never finished; the driver may have been stopped \
                 mid-pass",
                wake.id,
                render_time(wake.started_at)
            ),
        ),
        Some(wake) => push(
            "last_os_wake",
            "ok",
            format!(
                "wake #{} started {} with {} claimed, {} succeeded, {} failed, {} unknown",
                wake.id,
                render_time(wake.started_at),
                wake.claimed,
                wake.succeeded,
                wake.failed,
                wake.unknown
            ),
        ),
        None => push(
            "last_os_wake",
            "warn",
            "no OS wake has been recorded yet".into(),
        ),
    }

    let healthy = checks.iter().all(|check| check.severity != "fail");
    (healthy, checks)
}

fn action_from_wire(action: ScheduleActionWire) -> Result<ScheduleAction, ScheduleError> {
    match action {
        ScheduleActionWire::Shell { command } => Ok(ScheduleAction::Shell { command }),
        ScheduleActionWire::Native { action, input } => {
            let input = if input.trim().is_empty() {
                serde_json::json!({})
            } else {
                serde_json::from_str(&input).map_err(|error| {
                    ScheduleError::Invalid(format!(
                        "the native action input is not valid JSON: {error}"
                    ))
                })?
            };
            Ok(ScheduleAction::Native { action, input })
        }
        ScheduleActionWire::Agent { goal } => Ok(ScheduleAction::Agent { goal }),
    }
}

fn action_wire(action: &ScheduleAction) -> Result<ScheduleActionWire, ScheduleError> {
    match action {
        ScheduleAction::Shell { command } => Ok(ScheduleActionWire::Shell {
            command: command.clone(),
        }),
        ScheduleAction::Native { action, input } => Ok(ScheduleActionWire::Native {
            action: action.clone(),
            input: serde_json::to_string(input).map_err(|error| {
                ScheduleError::Invalid(format!(
                    "the stored native action input is not serializable: {error}"
                ))
            })?,
        }),
        ScheduleAction::Agent { goal } => Ok(ScheduleActionWire::Agent { goal: goal.clone() }),
    }
}

fn trigger_from_wire(trigger: ScheduleTriggerWire) -> Result<ScheduleTrigger, ScheduleError> {
    match trigger {
        ScheduleTriggerWire::Once { at } => Ok(ScheduleTrigger::Once {
            at: parse_time(&at)?,
        }),
        ScheduleTriggerWire::Every { every_secs, anchor } => Ok(ScheduleTrigger::Every {
            anchor: match anchor {
                Some(anchor) => parse_time(&anchor)?,
                None => leveler_core::now(),
            },
            every: Duration::from_secs(every_secs),
        }),
        ScheduleTriggerWire::Daily { time, timezone } => Ok(ScheduleTrigger::Daily {
            time: LocalTime::parse(&time)?,
            timezone: rule_timezone(timezone.as_deref())?,
        }),
        ScheduleTriggerWire::Weekly {
            weekdays,
            time,
            timezone,
        } => Ok(ScheduleTrigger::Weekly {
            weekdays: WeekdaySet::parse(&weekdays)?,
            time: LocalTime::parse(&time)?,
            timezone: rule_timezone(timezone.as_deref())?,
        }),
        ScheduleTriggerWire::Cron {
            expression,
            timezone,
        } => {
            parse_cron(&expression)?;
            Ok(ScheduleTrigger::Cron {
                expression,
                timezone: rule_timezone(timezone.as_deref())?,
            })
        }
    }
}

fn trigger_wire(trigger: &ScheduleTrigger) -> ScheduleTriggerWire {
    match trigger {
        ScheduleTrigger::Once { at } => ScheduleTriggerWire::Once {
            at: render_time(*at),
        },
        ScheduleTrigger::Every { anchor, every } => ScheduleTriggerWire::Every {
            every_secs: every.as_secs(),
            anchor: Some(render_time(*anchor)),
        },
        ScheduleTrigger::Daily { time, timezone } => ScheduleTriggerWire::Daily {
            time: time.to_string(),
            timezone: Some(timezone.to_string()),
        },
        ScheduleTrigger::Weekly {
            weekdays,
            time,
            timezone,
        } => ScheduleTriggerWire::Weekly {
            weekdays: weekdays.to_string(),
            time: time.to_string(),
            timezone: Some(timezone.to_string()),
        },
        ScheduleTrigger::Cron {
            expression,
            timezone,
        } => ScheduleTriggerWire::Cron {
            expression: expression.clone(),
            timezone: Some(timezone.to_string()),
        },
    }
}

fn rule_timezone(explicit: Option<&str>) -> Result<chrono_tz::Tz, ScheduleError> {
    match explicit {
        Some(raw) => parse_timezone(raw),
        None => host_timezone(),
    }
}

fn parse_time(raw: &str) -> Result<DateTime<Utc>, ScheduleError> {
    DateTime::parse_from_rfc3339(raw.trim())
        .map(|value| value.with_timezone(&Utc))
        .map_err(|error| ScheduleError::Invalid(format!("invalid timestamp `{raw}`: {error}")))
}

fn parse_revision(raw: &str) -> Result<DateTime<Utc>, ScheduleError> {
    parse_time(raw)
}

fn parse_id(raw: &str) -> Result<ScheduleId, ScheduleError> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Err(ScheduleError::Invalid("the schedule id is empty".into()));
    }
    if trimmed.len() > MAX_ID_BYTES {
        return Err(ScheduleError::Invalid("the schedule id is too long".into()));
    }
    if !trimmed
        .chars()
        .all(|ch| ch.is_ascii_alphanumeric() || ch == '-' || ch == '_')
    {
        return Err(ScheduleError::Invalid(
            "the schedule id contains an unexpected character".into(),
        ));
    }
    Ok(ScheduleId::new(trimmed))
}

fn parse_model(raw: &str) -> Result<Option<ModelRef>, ScheduleError> {
    if raw.trim().is_empty() {
        return Ok(None);
    }
    ModelRef::parse(raw.trim()).map(Some).ok_or_else(|| {
        ScheduleError::Invalid(format!(
            "`{raw}` is not a provider/model reference; the runtime never guesses a model"
        ))
    })
}

fn parse_session_policy(raw: &str) -> Result<SessionPolicy, ScheduleError> {
    SessionPolicy::parse(raw).ok_or_else(|| {
        ScheduleError::Invalid(format!(
            "`{raw}` is not a session policy; use `continue` or `fresh`"
        ))
    })
}

fn parse_overlap(raw: &str) -> Result<OverlapPolicy, ScheduleError> {
    OverlapPolicy::parse(raw).ok_or_else(|| {
        ScheduleError::Invalid(format!(
            "`{raw}` is not an overlap policy; use `skip-if-running`, `queue-one` or \
             `allow-concurrent`"
        ))
    })
}

fn parse_misfire(raw: &str) -> Result<MisfirePolicy, ScheduleError> {
    MisfirePolicy::parse(raw).ok_or_else(|| {
        ScheduleError::Invalid(format!(
            "`{raw}` is not a misfire policy; use `skip` or `run-once`"
        ))
    })
}

fn parse_mode(raw: &str) -> Result<PermissionProfile, ScheduleError> {
    match raw {
        "request_approval" => Ok(PermissionProfile::RequestApproval),
        "assisted" => Ok(PermissionProfile::Assisted),
        "full_access" => Ok(PermissionProfile::FullAccess),
        other => Err(ScheduleError::Invalid(format!(
            "`{other}` is not a permission mode; use `request_approval`, `assisted` or \
             `full_access`"
        ))),
    }
}

fn render_time(value: DateTime<Utc>) -> String {
    value.to_rfc3339_opts(SecondsFormat::Millis, true)
}
