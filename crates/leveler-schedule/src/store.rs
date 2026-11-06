//! SQLite persistence for the global schedule domain.

use std::collections::HashMap;
use std::path::Path;
use std::time::Duration;

use chrono::{DateTime, Utc};
use leveler_client_protocol::{ModelRef, PermissionProfile, SessionId};
use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions};
use sqlx::{Pool, Row, Sqlite};

use crate::{
    DefinitionUpdate, DispatchSummary, MisfirePolicy, NewScheduleDefinition, Occurrence,
    OccurrenceId, OccurrenceState, OverlapPolicy, RunId, RunState, ScheduleAction,
    ScheduleDefinition, ScheduleError, ScheduleId, ScheduleRun, ScheduleTrigger, SessionPolicy,
    UnattendedGrant, UnattendedRequest, UsageSnapshot, WakeRecord, WakeSource, parse_timestamp,
    timestamp,
};

static MIGRATOR: sqlx::migrate::Migrator = sqlx::migrate!("./migrations");

/// One projection for every read of a definition. Keeping the column list in
/// one place prevents a newly persisted field from being silently dropped by
/// one of the readers.
const DEFINITION_COLUMNS: &str = "id, name, workspace, action, command, agent_goal, \
     native_action, native_input, trigger_kind, once_at, anchor_at, every_secs, timezone, \
     local_time, weekdays, cron_expr, session_policy, overlap_policy, permission_mode, model, \
     timeout_secs, misfire_policy, misfire_grace_secs, unattended_grant, session_id, enabled, \
     created_at, updated_at";

/// The same for runs: the occurrence's `scheduled_for` is joined because the
/// slot, not the run, owns it.
const RUN_COLUMNS: &str = "r.id, r.occurrence_id, r.definition_id, o.scheduled_for, r.state, \
     r.session_id, r.execution_id, r.exit_code, r.error, r.started_at, r.delivered_at, \
     r.running_at, r.finished_at, r.baseline_sequence, r.turn_ordinal, r.task_sequence, \
     r.task_outcome, \
     r.provider, r.native_outcome, r.capability_state, r.permission_state, r.usage_requests, \
     r.input_tokens, r.cached_input_tokens, r.output_tokens, r.reasoning_tokens, \
     r.cost_usd_micros, r.usage_rows_without_cost, r.handed_off_at, r.deadline_at, \
     r.cancel_requested_at, r.cancel_issued_at, r.usage_baseline_requests, \
     r.usage_baseline_input_tokens, r.usage_baseline_cached_input_tokens, \
     r.usage_baseline_output_tokens, r.usage_baseline_reasoning_tokens, \
     r.usage_baseline_cost_usd_micros, r.usage_baseline_rows_without_cost";

const RUN_FROM: &str = "schedule_runs r JOIN schedule_occurrences o ON o.id = r.occurrence_id";

/// Dispatch-pass observations older than this are pruned when a new pass
/// starts. A minute-cadence wake writes ~1,440 rows per day; the recent window
/// is what diagnostics and the misfire story need.
const WAKE_RETENTION_DAYS: i64 = 30;

/// A definition plus one atomically claimed occurrence and its new run.
#[derive(Debug, Clone)]
pub struct Claim {
    pub definition: ScheduleDefinition,
    pub occurrence: Occurrence,
    pub run_id: RunId,
}

/// One lease-recovered occurrence.
#[derive(Debug, Clone)]
pub struct Recovered {
    pub occurrence_id: OccurrenceId,
    pub definition_id: ScheduleId,
}

/// An agent run the dispatcher handed to the runtime and no longer owns. The
/// dispatcher reconnects on a later pass, observes the runtime's terminal
/// record, and only then settles it.
#[derive(Debug, Clone)]
pub struct HandedOffRun {
    pub run: ScheduleRun,
    pub definition: ScheduleDefinition,
}

/// Facts observed about a run when it settles. Every field is optional and
/// written only when it was actually observed; absence stays absence.
#[derive(Debug, Clone, Default)]
pub struct RunFacts {
    pub task_sequence: Option<i64>,
    pub task_outcome: Option<String>,
    pub provider: Option<String>,
    pub native_outcome: Option<String>,
    pub capability_state: Option<String>,
    pub permission_state: Option<String>,
    /// Final totals in the agent session, not the run's delta.
    pub usage: Option<UsageSnapshot>,
    /// The run's own usage, already subtracted from the session baseline.
    pub usage_delta: Option<UsageSnapshot>,
}

/// The observed outcome of one run.
#[derive(Debug, Clone)]
pub enum RunOutcome {
    Succeeded {
        exit_code: Option<i32>,
    },
    /// A provider accepted the request; the final effect is unconfirmed.
    /// Never reported as success and never retried automatically.
    Submitted {
        error: Option<String>,
    },
    Failed {
        exit_code: Option<i32>,
        error: String,
    },
    /// The effect cannot be proven. The occurrence is never retried.
    Unknown {
        error: String,
    },
    /// The overlap policy suppressed this slot before any delivery.
    Skipped {
        error: String,
    },
    /// Execution stopped because an unattended decision could not be made.
    /// Nothing was started; a later explicit attempt may succeed.
    WaitingApproval {
        error: String,
    },
    /// A precondition or authorization refused this run. Nothing was started.
    Blocked {
        error: String,
    },
    /// The runtime observed cancellation. Distinct from `Unknown`.
    Cancelled {
        error: Option<String>,
    },
}

impl RunOutcome {
    fn state(&self) -> RunState {
        match self {
            Self::Succeeded { .. } => RunState::Succeeded,
            Self::Submitted { .. } => RunState::Submitted,
            Self::Failed { .. } => RunState::Failed,
            Self::Unknown { .. } => RunState::Unknown,
            Self::Skipped { .. } => RunState::Skipped,
            Self::WaitingApproval { .. } => RunState::WaitingApproval,
            Self::Blocked { .. } => RunState::Blocked,
            Self::Cancelled { .. } => RunState::Cancelled,
        }
    }

    fn exit_code(&self) -> Option<i32> {
        match self {
            Self::Succeeded { exit_code } | Self::Failed { exit_code, .. } => *exit_code,
            _ => None,
        }
    }

    fn error(&self) -> Option<&str> {
        match self {
            Self::Succeeded { .. } => None,
            Self::Failed { error, .. }
            | Self::Unknown { error }
            | Self::Skipped { error }
            | Self::WaitingApproval { error }
            | Self::Blocked { error } => Some(error),
            Self::Submitted { error } | Self::Cancelled { error } => error.as_deref(),
        }
    }

    /// The occurrence state this run leaves behind. A run always settles its
    /// own occurrence; policy is reflected in both records, never in only one.
    fn occurrence_state(&self) -> OccurrenceState {
        match self {
            Self::Succeeded { .. }
            | Self::Submitted { .. }
            | Self::Failed { .. }
            | Self::Unknown { .. } => OccurrenceState::Completed,
            Self::Skipped { .. } => OccurrenceState::Skipped,
            Self::WaitingApproval { .. } => OccurrenceState::WaitingApproval,
            Self::Blocked { .. } => OccurrenceState::Blocked,
            Self::Cancelled { .. } => OccurrenceState::Cancelled,
        }
    }
}

/// The global schedule store. One SQLite file under the Leveler home.
#[derive(Debug, Clone)]
pub struct ScheduleStore {
    pool: Pool<Sqlite>,
}

impl ScheduleStore {
    /// Open (creating if needed) the store and apply its own migrations.
    pub async fn open(path: &Path) -> Result<Self, ScheduleError> {
        if let Some(parent) = path.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        let options = SqliteConnectOptions::new()
            .filename(path)
            .create_if_missing(true)
            .journal_mode(SqliteJournalMode::Wal)
            .busy_timeout(Duration::from_secs(5))
            .foreign_keys(true);
        // Several clients (the TUI, a CLI command, a dispatcher pass) may open
        // a brand-new store at the same time. The WAL switch and sqlx's
        // migration ledger both take short write locks, and the loser of that
        // race observes the winner's state (`database is locked`, `table ...
        // already exists`, `duplicate column name`). Retrying the whole open is
        // idempotent: a converged store skips its applied ledger rows, while a
        // genuinely invalid migration fails identically and is returned.
        let mut last_error = None;
        for attempt in 0..20 {
            match Self::open_once(&options).await {
                Ok(store) => return Ok(store),
                Err(error) => {
                    last_error = Some(error);
                    if attempt + 1 < 20 {
                        tokio::time::sleep(Duration::from_millis(25)).await;
                    }
                }
            }
        }
        Err(last_error.expect("the open loop runs at least once"))
    }

    async fn open_once(options: &SqliteConnectOptions) -> Result<Self, ScheduleError> {
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect_with(options.clone())
            .await?;
        if let Err(error) = MIGRATOR.run(&pool).await {
            // Release the handle before the caller retries so this attempt
            // cannot keep the database lock it just failed to acquire.
            pool.close().await;
            return Err(error.into());
        }
        Ok(Self { pool })
    }

    #[cfg(test)]
    pub(crate) async fn open_in_memory() -> Result<Self, ScheduleError> {
        let options = SqliteConnectOptions::new()
            .in_memory(true)
            .foreign_keys(true);
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect_with(options)
            .await?;
        MIGRATOR.run(&pool).await?;
        Ok(Self { pool })
    }

    /// Persist a validated definition and its first occurrence atomically.
    pub async fn create_definition(
        &self,
        new: NewScheduleDefinition,
    ) -> Result<ScheduleDefinition, ScheduleError> {
        new.validate()?;
        if let Some(request) = new.unattended {
            validate_unattended_request(&new.action, request)?;
        }
        // The workspace names the runtime source that owns this schedule. The
        // runtime compares it by its canonical identity, so the durable value
        // must be canonical too; otherwise a macOS `/var` spelling would bind
        // the schedule to a source that refuses it.
        let workspace = tokio::fs::canonicalize(&new.workspace)
            .await
            .map_err(|error| {
                ScheduleError::Invalid(format!(
                    "the schedule workspace `{}` is not accessible: {error}",
                    new.workspace.display()
                ))
            })?;
        if !tokio::fs::metadata(&workspace).await?.is_dir() {
            return Err(ScheduleError::Invalid(format!(
                "the schedule workspace `{}` is not a directory",
                workspace.display()
            )));
        }
        let now = leveler_core::now();
        let id = ScheduleId::generate();
        let first = new.trigger.first_occurrence(now)?;
        let grant = new
            .unattended
            .map(|request| {
                UnattendedGrant::new(
                    id.clone(),
                    workspace.clone(),
                    new.mode,
                    &new.action,
                    &new.trigger,
                    new.model.clone(),
                    request,
                    now,
                )
            })
            .transpose()?;
        let grant_json = grant
            .as_ref()
            .map(serde_json::to_string)
            .transpose()
            .map_err(|error| {
                ScheduleError::Invalid(format!("the unattended grant is not serializable: {error}"))
            })?;
        let action_columns = action_columns(&new.action)?;
        let trigger_columns = trigger_columns(&new.trigger);
        let (native_action, native_input, agent_goal) = (
            action_columns.native_action,
            action_columns.native_input,
            action_columns.agent_goal,
        );
        let trigger_kind = trigger_columns.kind;
        let once_at = trigger_columns.once_at;
        let anchor_at = trigger_columns.anchor_at;
        let every_secs = trigger_columns.every_secs;
        let timezone = trigger_columns.timezone;
        let local_time = trigger_columns.local_time;
        let weekdays = trigger_columns.weekdays;
        let cron_expr = trigger_columns.cron_expr;
        let mut tx = self.pool.begin().await?;
        sqlx::query(
            "INSERT INTO schedule_definitions \
             (id, name, workspace, action, command, agent_goal, native_action, native_input, \
              trigger_kind, once_at, anchor_at, every_secs, \
              timezone, local_time, weekdays, cron_expr, \
              session_policy, overlap_policy, permission_mode, model, timeout_secs, \
              misfire_policy, misfire_grace_secs, unattended_grant, \
              session_id, enabled, created_at, updated_at) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, \
                     NULL, 1, ?, ?)",
        )
        .bind(id.as_str())
        .bind(new.name.trim())
        .bind(workspace.display().to_string())
        .bind(action_columns.kind)
        .bind(new.action.command())
        .bind(agent_goal)
        .bind(native_action)
        .bind(native_input)
        .bind(trigger_kind)
        .bind(once_at)
        .bind(anchor_at)
        .bind(every_secs)
        .bind(timezone)
        .bind(local_time)
        .bind(weekdays)
        .bind(cron_expr)
        .bind(new.session_policy.as_str())
        .bind(new.overlap.as_str())
        .bind(new.mode.as_str())
        .bind(new.model.as_ref().map(|model| model.to_string()))
        .bind(new.timeout.as_secs() as i64)
        .bind(new.misfire.as_str())
        .bind(new.misfire_grace.as_secs() as i64)
        .bind(grant_json)
        .bind(timestamp(now))
        .bind(timestamp(now))
        .execute(&mut *tx)
        .await?;
        sqlx::query(
            "INSERT INTO schedule_occurrences \
             (id, definition_id, scheduled_for, state, created_at) VALUES (?, ?, ?, 'pending', ?)",
        )
        .bind(OccurrenceId::generate().as_str())
        .bind(id.as_str())
        .bind(timestamp(first))
        .bind(timestamp(now))
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        self.definition(&id)
            .await?
            .ok_or_else(|| ScheduleError::NotFound(id))
    }

    /// Every definition, oldest first.
    pub async fn definitions(&self) -> Result<Vec<ScheduleDefinition>, ScheduleError> {
        let rows = sqlx::query(&format!(
            "SELECT {DEFINITION_COLUMNS} FROM schedule_definitions ORDER BY created_at, id"
        ))
        .fetch_all(&self.pool)
        .await?;
        rows.iter().map(definition_from_row).collect()
    }

    /// One definition by id.
    pub async fn definition(
        &self,
        id: &ScheduleId,
    ) -> Result<Option<ScheduleDefinition>, ScheduleError> {
        let row = sqlx::query(&format!(
            "SELECT {DEFINITION_COLUMNS} FROM schedule_definitions WHERE id = ?"
        ))
        .bind(id.as_str())
        .fetch_optional(&self.pool)
        .await?;
        row.as_ref().map(definition_from_row).transpose()
    }

    /// Delete a definition, its occurrences and their runs.
    ///
    /// Deletion never kills a task the runtime already started: while a run is
    /// active, removal is refused so the caller must make the cancellation an
    /// explicit, observable action first. The runtime task outlives neither a
    /// silent delete nor a rewritten history.
    pub async fn remove_definition(&self, id: &ScheduleId) -> Result<bool, ScheduleError> {
        let active: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM schedule_runs WHERE definition_id = ? \
             AND state IN ('dispatching', 'running')",
        )
        .bind(id.as_str())
        .fetch_one(&self.pool)
        .await?;
        if active > 0 {
            return Err(ScheduleError::Invalid(format!(
                "schedule {id} still has {active} active run(s); cancel it explicitly before \
                 deleting, so the runtime's task is settled rather than orphaned"
            )));
        }
        let result = sqlx::query("DELETE FROM schedule_definitions WHERE id = ?")
            .bind(id.as_str())
            .execute(&self.pool)
            .await?;
        Ok(result.rows_affected() == 1)
    }

    /// Enable or pause future triggers. An active run is intentionally not
    /// affected: pausing says "stop creating slots", not "kill the task".
    pub async fn set_enabled(&self, id: &ScheduleId, enabled: bool) -> Result<bool, ScheduleError> {
        let result =
            sqlx::query("UPDATE schedule_definitions SET enabled = ?, updated_at = ? WHERE id = ?")
                .bind(if enabled { 1_i64 } else { 0_i64 })
                .bind(timestamp(leveler_core::now()))
                .bind(id.as_str())
                .execute(&self.pool)
                .await?;
        Ok(result.rows_affected() == 1)
    }

    /// Remove the persisted unattended grant. The next run re-evaluates
    /// authorization and refuses, because the grant is read from storage at
    /// dispatch time, not captured when the schedule was created.
    pub async fn revoke_unattended(&self, id: &ScheduleId) -> Result<bool, ScheduleError> {
        let result = sqlx::query(
            "UPDATE schedule_definitions SET unattended_grant = NULL, updated_at = ? \
             WHERE id = ? AND unattended_grant IS NOT NULL",
        )
        .bind(timestamp(leveler_core::now()))
        .bind(id.as_str())
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected() == 1)
    }

    /// Replace a definition in place, keeping its id and run history.
    ///
    /// The update is a full declaration; the management layer resolves a
    /// partial patch against the definition the client observed. When
    /// `expected_updated_at` is supplied and no longer matches the stored row,
    /// the update is refused with [`ScheduleError::Conflict`] instead of
    /// overwriting a newer definition.
    ///
    /// A grant survives only while it still covers the new identity (action,
    /// workspace, permission mode and model). Anything the user changed that
    /// the grant named drops the grant, so the next unattended run refuses
    /// until the user re-authorizes; it is never silently widened.
    pub async fn update_definition(
        &self,
        id: &ScheduleId,
        update: DefinitionUpdate,
        expected_updated_at: Option<DateTime<Utc>>,
    ) -> Result<ScheduleDefinition, ScheduleError> {
        update.validate()?;
        if let Some(request) = update.grant {
            validate_unattended_request(&update.action, request)?;
        }
        let existing = self
            .definition(id)
            .await?
            .ok_or_else(|| ScheduleError::NotFound(id.clone()))?;
        if let Some(expected) = expected_updated_at
            && timestamp(expected) != timestamp(existing.updated_at)
        {
            return Err(ScheduleError::Conflict(id.clone()));
        }
        let workspace = tokio::fs::canonicalize(&update.workspace)
            .await
            .map_err(|error| {
                ScheduleError::Invalid(format!(
                    "the schedule workspace `{}` is not accessible: {error}",
                    update.workspace.display()
                ))
            })?;
        if !tokio::fs::metadata(&workspace).await?.is_dir() {
            return Err(ScheduleError::Invalid(format!(
                "the schedule workspace `{}` is not a directory",
                workspace.display()
            )));
        }
        // `updated_at` is the client's revision. Stored with millisecond
        // precision, so two updates inside one millisecond must still produce
        // distinct revisions or a later stale write would compare equal.
        let now = std::cmp::max(
            leveler_core::now(),
            existing.updated_at + chrono::Duration::milliseconds(1),
        );
        let trigger_changed = existing.trigger != update.trigger;
        let candidate = ScheduleDefinition {
            id: id.clone(),
            name: update.name.trim().to_string(),
            workspace: workspace.clone(),
            action: update.action.clone(),
            trigger: update.trigger.clone(),
            session_policy: update.session_policy,
            overlap: update.overlap,
            mode: update.mode,
            model: update.model.clone(),
            timeout: update.timeout,
            misfire: update.misfire,
            misfire_grace: update.misfire_grace,
            session_id: if workspace == existing.workspace {
                existing.session_id.clone()
            } else {
                // A session belongs to the workspace database that owns it.
                // Carrying the id across a workspace change would name a
                // session the new runtime cannot resolve.
                None
            },
            unattended: None,
            enabled: existing.enabled,
            created_at: existing.created_at,
            updated_at: now,
        };
        let grant = match (update.grant, existing.unattended.as_ref()) {
            (Some(request), _) => Some(UnattendedGrant::new(
                id.clone(),
                workspace.clone(),
                update.mode,
                &update.action,
                &update.trigger,
                update.model.clone(),
                request,
                now,
            )?),
            (None, Some(current)) => current
                .covers(&candidate, update.action.kind(), false)
                .ok()
                .map(|()| current.clone()),
            (None, None) => None,
        };
        let grant_json = grant
            .as_ref()
            .map(serde_json::to_string)
            .transpose()
            .map_err(|error| {
                ScheduleError::Invalid(format!("the unattended grant is not serializable: {error}"))
            })?;
        let action_columns = action_columns(&update.action)?;
        let trigger_columns = trigger_columns(&update.trigger);
        // A trigger change re-declares the rule. The replacement slot comes
        // from the new rule itself: `next_after` is the wrong question for a
        // one-off, because a one-off's slot is its declared instant and it has
        // no successor. A rule that offers no slot at all is an error here,
        // never a successful update with nothing left to run.
        let replacement = if trigger_changed {
            Some((
                OccurrenceId::generate(),
                update.trigger.replacement_occurrence(now)?,
            ))
        } else {
            None
        };
        let mut tx = self.pool.begin().await?;
        let result = sqlx::query(
            "UPDATE schedule_definitions SET \
             name = ?, workspace = ?, action = ?, command = ?, agent_goal = ?, \
             native_action = ?, native_input = ?, \
             trigger_kind = ?, once_at = ?, anchor_at = ?, every_secs = ?, \
             timezone = ?, local_time = ?, weekdays = ?, cron_expr = ?, \
             session_policy = ?, overlap_policy = ?, permission_mode = ?, model = ?, \
             timeout_secs = ?, misfire_policy = ?, misfire_grace_secs = ?, \
             unattended_grant = ?, session_id = ?, updated_at = ? \
             WHERE id = ? AND updated_at = ?",
        )
        .bind(candidate.name.as_str())
        .bind(workspace.display().to_string())
        .bind(action_columns.kind)
        .bind(update.action.command())
        .bind(action_columns.agent_goal)
        .bind(action_columns.native_action)
        .bind(action_columns.native_input)
        .bind(trigger_columns.kind)
        .bind(trigger_columns.once_at)
        .bind(trigger_columns.anchor_at)
        .bind(trigger_columns.every_secs)
        .bind(trigger_columns.timezone)
        .bind(trigger_columns.local_time)
        .bind(trigger_columns.weekdays)
        .bind(trigger_columns.cron_expr)
        .bind(update.session_policy.as_str())
        .bind(update.overlap.as_str())
        .bind(update.mode.as_str())
        .bind(update.model.as_ref().map(ModelRef::to_string))
        .bind(update.timeout.as_secs() as i64)
        .bind(update.misfire.as_str())
        .bind(update.misfire_grace.as_secs() as i64)
        .bind(grant_json)
        .bind(candidate.session_id.as_ref().map(SessionId::as_str))
        .bind(timestamp(now))
        .bind(id.as_str())
        // The expected revision is checked again inside the transaction: the
        // row could have changed between the read above and this write.
        .bind(timestamp(existing.updated_at))
        .execute(&mut *tx)
        .await?;
        if result.rows_affected() != 1 {
            tx.rollback().await?;
            return Err(ScheduleError::Conflict(id.clone()));
        }
        // Slots generated by the old rule no longer describe the schedule the
        // user now owns. Explicit run-now slots are user requests and stay. An
        // unchanged rule keeps its pending slot untouched: re-deriving it would
        // shift or drop a slot (an interval's anchor, a pending one-off) for a
        // change that had nothing to do with timing.
        if let Some((occurrence_id, first)) = replacement {
            sqlx::query(
                "DELETE FROM schedule_occurrences \
                 WHERE definition_id = ? AND state = 'pending' AND run_now = 0",
            )
            .bind(id.as_str())
            .execute(&mut *tx)
            .await?;
            // The unique `(definition, scheduled_for)` key is the occurrence's
            // identity. If that instant already has a terminal row, inserting
            // a second one would rewrite history; ignoring the insert would
            // leave the updated definition with no next run. Neither is a
            // success, so the whole update rolls back.
            let inserted = sqlx::query(
                "INSERT OR IGNORE INTO schedule_occurrences \
                 (id, definition_id, scheduled_for, state, created_at) \
                 VALUES (?, ?, ?, 'pending', ?)",
            )
            .bind(occurrence_id.as_str())
            .bind(id.as_str())
            .bind(timestamp(first))
            .bind(timestamp(now))
            .execute(&mut *tx)
            .await?;
            if inserted.rows_affected() != 1 {
                tx.rollback().await?;
                return Err(ScheduleError::Invalid(format!(
                    "the new trigger's occurrence at {} already exists in this schedule's \
                     history; the update was not applied and the definition is unchanged",
                    timestamp(first)
                )));
            }
        }
        tx.commit().await?;
        self.definition(id)
            .await?
            .ok_or_else(|| ScheduleError::NotFound(id.clone()))
    }

    /// Create an explicit one-off slot that is claimable even while paused.
    /// It is an ordinary occurrence in every other respect: deduplicated,
    /// policy-bound and visible in history.
    pub async fn create_run_now(
        &self,
        definition: &ScheduleDefinition,
    ) -> Result<Occurrence, ScheduleError> {
        let now = leveler_core::now();
        let id = OccurrenceId::generate();
        sqlx::query(
            "INSERT INTO schedule_occurrences \
             (id, definition_id, scheduled_for, state, run_now, created_at) \
             VALUES (?, ?, ?, 'pending', 1, ?)",
        )
        .bind(id.as_str())
        .bind(definition.id.as_str())
        .bind(timestamp(now))
        .bind(timestamp(now))
        .execute(&self.pool)
        .await?;
        self.occurrence(&id)
            .await?
            .ok_or_else(|| ScheduleError::NotFound(definition.id.clone()))
    }

    /// Bind the definition's durable session. A concurrent winner's value is
    /// returned; a different existing value is a hard error, never replaced.
    pub async fn set_session(
        &self,
        id: &ScheduleId,
        session_id: &SessionId,
    ) -> Result<SessionId, ScheduleError> {
        let now = leveler_core::now();
        sqlx::query(
            "UPDATE schedule_definitions SET session_id = ?, updated_at = ? \
             WHERE id = ? AND session_id IS NULL",
        )
        .bind(session_id.as_str())
        .bind(timestamp(now))
        .bind(id.as_str())
        .execute(&self.pool)
        .await?;
        let stored: Option<Option<String>> =
            sqlx::query_scalar("SELECT session_id FROM schedule_definitions WHERE id = ?")
                .bind(id.as_str())
                .fetch_optional(&self.pool)
                .await?;
        match stored {
            None => Err(ScheduleError::NotFound(id.clone())),
            Some(Some(stored)) if stored == session_id.as_str() => Ok(session_id.clone()),
            Some(Some(stored)) => Err(ScheduleError::SessionMismatch(format!(
                "definition {id} is already bound to session {stored}, not {session_id}"
            ))),
            Some(None) => Err(ScheduleError::SessionMismatch(format!(
                "definition {id} did not accept its session binding"
            ))),
        }
    }

    /// Mark due slots suppressed by `skip-if-running` before they are claimed.
    ///
    /// The row is a terminal fact with its own skipped run: an operator can
    /// see that the trigger fired and why nothing was executed. `run_now`
    /// slots are explicit requests and are still subject to the definition's
    /// overlap policy; they are not silently dropped.
    pub async fn reconcile_overlaps(&self, now: DateTime<Utc>) -> Result<usize, ScheduleError> {
        let candidates: Vec<(String, String, String)> = sqlx::query_as(
            "SELECT o.id, o.definition_id, o.scheduled_for FROM schedule_occurrences o \
             JOIN schedule_definitions d ON d.id = o.definition_id \
             WHERE o.state = 'pending' AND o.scheduled_for <= ? \
               AND (d.enabled = 1 OR o.run_now = 1) \
               AND d.overlap_policy = 'skip-if-running' \
               AND EXISTS (SELECT 1 FROM schedule_runs active \
                           WHERE active.definition_id = d.id \
                             AND active.state IN ('dispatching', 'running')) \
             ORDER BY o.scheduled_for, o.id",
        )
        .bind(timestamp(now))
        .fetch_all(&self.pool)
        .await?;

        let mut skipped = 0;
        for (occurrence_id, definition_id, scheduled_for) in candidates {
            let mut tx = self.pool.begin().await?;
            let updated = sqlx::query(
                "UPDATE schedule_occurrences SET state = 'skipped' \
                 WHERE id = ? AND state = 'pending'",
            )
            .bind(&occurrence_id)
            .execute(&mut *tx)
            .await?;
            if updated.rows_affected() != 1 {
                tx.rollback().await?;
                continue;
            }
            let reason = format!(
                "the slot scheduled for {scheduled_for} became due while an earlier run of \
                 this schedule was still active; the overlap policy is skip-if-running and \
                 nothing was executed"
            );
            sqlx::query(
                "INSERT INTO schedule_runs \
                 (id, occurrence_id, definition_id, state, error, started_at, finished_at) \
                 VALUES (?, ?, ?, 'skipped', ?, ?, ?)",
            )
            .bind(RunId::generate().as_str())
            .bind(&occurrence_id)
            .bind(&definition_id)
            .bind(&reason)
            .bind(timestamp(now))
            .bind(timestamp(now))
            .execute(&mut *tx)
            .await?;
            tx.commit().await?;
            skipped += 1;
            if let Some(definition) = self.definition(&ScheduleId::from(definition_id)).await? {
                self.ensure_next_occurrence(&definition, now).await?;
            }
        }
        Ok(skipped)
    }

    /// Pending occurrences that are due, claimed atomically under `lease`.
    /// Candidates that lose the compare-and-set are skipped, not retried here.
    ///
    /// `queue-one` leaves its slot pending while any run of the same
    /// definition is active; it becomes claimable on the first pass after the
    /// active run settles. `skip-if-running` slots are normally settled by
    /// [`Self::reconcile_overlaps`]; the same predicate here closes the race
    /// between reconciliation and the claim.
    pub async fn claim_due(
        &self,
        now: DateTime<Utc>,
        lease: Duration,
        limit: usize,
    ) -> Result<Vec<Claim>, ScheduleError> {
        let candidates: Vec<(String, String)> = sqlx::query_as(
            "SELECT o.id, o.definition_id FROM schedule_occurrences o \
             JOIN schedule_definitions d ON d.id = o.definition_id \
             WHERE o.state = 'pending' AND o.scheduled_for <= ? \
               AND (d.enabled = 1 OR o.run_now = 1) \
               AND (d.overlap_policy = 'allow-concurrent' OR NOT EXISTS ( \
                     SELECT 1 FROM schedule_runs active \
                     WHERE active.definition_id = d.id \
                       AND active.state IN ('dispatching', 'running'))) \
             ORDER BY o.scheduled_for, o.id LIMIT ?",
        )
        .bind(timestamp(now))
        .bind(limit as i64)
        .fetch_all(&self.pool)
        .await?;

        let mut claims = Vec::new();
        for (occurrence_id, definition_id) in candidates {
            let mut tx = self.pool.begin().await?;
            // The overlap predicate is part of the write, not only the read
            // above: a pass that sees two due slots of one definition must not
            // claim both. SQLite serializes writers, so the second claim sees
            // the first claim's active run and leaves its slot pending.
            let updated = sqlx::query(
                "UPDATE schedule_occurrences SET state = 'claimed', claimed_at = ?, \
                 lease_expires_at = ? \
                 WHERE id = ? AND state = 'pending' \
                   AND EXISTS (SELECT 1 FROM schedule_definitions d WHERE d.id = ? \
                               AND (d.overlap_policy = 'allow-concurrent' OR NOT EXISTS ( \
                                     SELECT 1 FROM schedule_runs active \
                                     WHERE active.definition_id = d.id \
                                       AND active.state IN ('dispatching', 'running'))))",
            )
            .bind(timestamp(now))
            .bind(timestamp(lease_until(now, lease)))
            .bind(&occurrence_id)
            .bind(&definition_id)
            .execute(&mut *tx)
            .await?;
            if updated.rows_affected() != 1 {
                tx.rollback().await?;
                continue;
            }
            let run_id = RunId::generate();
            sqlx::query(
                "INSERT INTO schedule_runs \
                 (id, occurrence_id, definition_id, state, started_at) \
                 VALUES (?, ?, ?, 'dispatching', ?)",
            )
            .bind(run_id.as_str())
            .bind(&occurrence_id)
            .bind(&definition_id)
            .bind(timestamp(now))
            .execute(&mut *tx)
            .await?;
            tx.commit().await?;

            let Some(occurrence) = self.occurrence(&OccurrenceId::from(occurrence_id)).await?
            else {
                continue;
            };
            let Some(definition) = self.definition(&ScheduleId::from(definition_id)).await? else {
                continue;
            };
            claims.push(Claim {
                definition,
                occurrence,
                run_id,
            });
        }
        Ok(claims)
    }

    /// Claim one explicit run-now slot, ignoring its scheduled time.
    ///
    /// This exists so `run-now` cannot be starved by unrelated overdue slots:
    /// the user asked for this occurrence, not for a generic pass. Overlap is
    /// still enforced by the same predicate as [`Self::claim_due`]; a caller
    /// that wants the skip-if-running verdict recorded runs
    /// [`Self::reconcile_overlaps`] first.
    pub async fn claim_run_now(
        &self,
        occurrence_id: &OccurrenceId,
        now: DateTime<Utc>,
        lease: Duration,
    ) -> Result<Option<Claim>, ScheduleError> {
        let candidate: Option<(String, String)> = sqlx::query_as(
            "SELECT o.id, o.definition_id FROM schedule_occurrences o \
             JOIN schedule_definitions d ON d.id = o.definition_id \
             WHERE o.id = ? AND o.state = 'pending' AND o.run_now = 1 \
               AND (d.overlap_policy = 'allow-concurrent' OR NOT EXISTS ( \
                     SELECT 1 FROM schedule_runs active \
                     WHERE active.definition_id = d.id \
                       AND active.state IN ('dispatching', 'running')))",
        )
        .bind(occurrence_id.as_str())
        .fetch_optional(&self.pool)
        .await?;
        let Some((occurrence_id, definition_id)) = candidate else {
            return Ok(None);
        };

        let mut tx = self.pool.begin().await?;
        // Re-check the overlap predicate inside the write. The read above can
        // race another dispatcher claiming a different slot of the same
        // definition; the write serialization is what makes it exclusive.
        let updated = sqlx::query(
            "UPDATE schedule_occurrences SET state = 'claimed', claimed_at = ?, \
             lease_expires_at = ? \
             WHERE id = ? AND state = 'pending' \
               AND EXISTS (SELECT 1 FROM schedule_definitions d WHERE d.id = ? \
                           AND (d.overlap_policy = 'allow-concurrent' OR NOT EXISTS ( \
                                 SELECT 1 FROM schedule_runs active \
                                 WHERE active.definition_id = d.id \
                                   AND active.state IN ('dispatching', 'running'))))",
        )
        .bind(timestamp(now))
        .bind(timestamp(lease_until(now, lease)))
        .bind(&occurrence_id)
        .bind(&definition_id)
        .execute(&mut *tx)
        .await?;
        if updated.rows_affected() != 1 {
            tx.rollback().await?;
            return Ok(None);
        }
        let run_id = RunId::generate();
        sqlx::query(
            "INSERT INTO schedule_runs \
             (id, occurrence_id, definition_id, state, started_at) \
             VALUES (?, ?, ?, 'dispatching', ?)",
        )
        .bind(run_id.as_str())
        .bind(&occurrence_id)
        .bind(&definition_id)
        .bind(timestamp(now))
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;

        let Some(occurrence) = self.occurrence(&OccurrenceId::from(occurrence_id)).await? else {
            return Ok(None);
        };
        let Some(definition) = self.definition(&ScheduleId::from(definition_id)).await? else {
            return Ok(None);
        };
        Ok(Some(Claim {
            definition,
            occurrence,
            run_id,
        }))
    }

    /// Recover claims whose dispatcher did not renew its lease. The occurrence
    /// is interrupted, its run becomes `unknown`, and a recurring definition's
    /// next future slot is created. Nothing is re-executed.
    pub async fn recover_expired(
        &self,
        now: DateTime<Utc>,
    ) -> Result<Vec<Recovered>, ScheduleError> {
        let candidates: Vec<(String, String)> = sqlx::query_as(
            "SELECT id, definition_id FROM schedule_occurrences \
             WHERE state = 'claimed' AND lease_expires_at IS NOT NULL AND lease_expires_at <= ?",
        )
        .bind(timestamp(now))
        .fetch_all(&self.pool)
        .await?;

        let mut recovered = Vec::new();
        for (occurrence_id, definition_id) in candidates {
            let mut tx = self.pool.begin().await?;
            let interrupted = sqlx::query(
                "UPDATE schedule_occurrences SET state = 'interrupted' \
                 WHERE id = ? AND state = 'claimed'",
            )
            .bind(&occurrence_id)
            .execute(&mut *tx)
            .await?;
            if interrupted.rows_affected() != 1 {
                tx.rollback().await?;
                continue;
            }
            // Say which fact was lost: a claim whose delivery was never
            // acknowledged is a different risk from an execution the runtime
            // had already started. Both remain unknown; neither is retried.
            let facts: Option<(Option<String>, Option<String>)> = sqlx::query_as(
                "SELECT delivered_at, execution_id FROM schedule_runs \
                 WHERE occurrence_id = ? AND state IN ('dispatching', 'running')",
            )
            .bind(&occurrence_id)
            .fetch_optional(&mut *tx)
            .await?;
            let lost = match facts {
                Some((_, Some(_))) => "the runtime had already started this action",
                Some((Some(_), None)) => {
                    "the runtime acknowledged delivery but never reported the execution"
                }
                _ => "delivery was never acknowledged",
            };
            sqlx::query(
                "UPDATE schedule_runs SET state = 'unknown', error = ?, finished_at = ? \
                 WHERE occurrence_id = ? AND state IN ('dispatching', 'running')",
            )
            .bind(format!(
                "the dispatcher's lease expired before the run reported a terminal state; \
                 {lost}; the action's effect is unknown and it was not retried"
            ))
            .bind(timestamp(now))
            .bind(&occurrence_id)
            .execute(&mut *tx)
            .await?;
            tx.commit().await?;

            let definition = self
                .definition(&ScheduleId::from(definition_id.clone()))
                .await?;
            if let Some(definition) = &definition {
                self.ensure_next_occurrence(definition, now).await?;
            }
            recovered.push(Recovered {
                occurrence_id: OccurrenceId::from(occurrence_id),
                definition_id: ScheduleId::from(definition_id),
            });
        }
        Ok(recovered)
    }

    /// Record the durable session on a claimed run.
    pub async fn mark_session(
        &self,
        run_id: &RunId,
        session_id: &SessionId,
    ) -> Result<(), ScheduleError> {
        sqlx::query(
            "UPDATE schedule_runs SET session_id = ? WHERE id = ? AND state = 'dispatching'",
        )
        .bind(session_id.as_str())
        .bind(run_id.as_str())
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Apply the definition's misfire policy to pending slots whose time has
    /// passed by more than its grace.
    ///
    /// `Skip` marks the slot `missed`; `RunOnceOnRecovery` leaves it pending so
    /// the same dispatch pass may claim and execute it once. Only `pending`
    /// slots are candidates: a completed, interrupted or already claimed run
    /// is a terminal fact and is never rewritten by policy.
    pub async fn reconcile_misfires(&self, now: DateTime<Utc>) -> Result<usize, ScheduleError> {
        let candidates: Vec<(String, String, String, String, i64)> = sqlx::query_as(
            "SELECT o.id, o.definition_id, o.scheduled_for, d.misfire_policy, \
             d.misfire_grace_secs FROM schedule_occurrences o \
             JOIN schedule_definitions d ON d.id = o.definition_id \
             WHERE o.state = 'pending' AND o.scheduled_for <= ? AND d.enabled = 1 \
               AND o.run_now = 0 \
             ORDER BY o.definition_id, o.scheduled_for",
        )
        .bind(timestamp(now))
        .fetch_all(&self.pool)
        .await?;

        // `RunOnceOnRecovery` executes at most the newest overdue slot. Older
        // ones are recorded as missed in the same pass, so a long outage can
        // never turn into a burst of catch-up side effects.
        let mut newest_overdue: std::collections::HashMap<String, (String, DateTime<Utc>)> =
            std::collections::HashMap::new();
        for (occurrence_id, definition_id, scheduled_for, policy, grace_secs) in &candidates {
            if MisfirePolicy::parse(policy) != Some(MisfirePolicy::RunOnceOnRecovery) {
                continue;
            }
            let scheduled_for = parse_timestamp(scheduled_for)?;
            let grace = chrono::Duration::seconds((*grace_secs).max(0));
            if now - scheduled_for <= grace {
                continue;
            }
            match newest_overdue.get(definition_id) {
                Some((_, stored)) if *stored >= scheduled_for => {}
                _ => {
                    newest_overdue.insert(
                        definition_id.clone(),
                        (occurrence_id.clone(), scheduled_for),
                    );
                }
            }
        }

        let mut missed = 0;
        for (occurrence_id, definition_id, scheduled_for, policy, grace_secs) in candidates {
            let policy = MisfirePolicy::parse(&policy).ok_or_else(|| {
                ScheduleError::Invalid(format!("unsupported misfire policy `{policy}`"))
            })?;
            let scheduled_for = parse_timestamp(&scheduled_for)?;
            let grace = chrono::Duration::seconds(grace_secs.max(0));
            if now - scheduled_for <= grace {
                continue;
            }
            match policy {
                MisfirePolicy::RunOnceOnRecovery => {
                    if newest_overdue
                        .get(&definition_id)
                        .is_some_and(|(id, _)| *id == occurrence_id)
                    {
                        continue;
                    }
                }
                MisfirePolicy::Skip => {}
            }
            let mut tx = self.pool.begin().await?;
            let updated = sqlx::query(
                "UPDATE schedule_occurrences SET state = 'missed' \
                 WHERE id = ? AND state = 'pending'",
            )
            .bind(&occurrence_id)
            .execute(&mut *tx)
            .await?;
            if updated.rows_affected() != 1 {
                tx.rollback().await?;
                continue;
            }
            tx.commit().await?;
            missed += 1;
            if let Some(definition) = self.definition(&ScheduleId::from(definition_id)).await? {
                self.ensure_next_occurrence(&definition, now).await?;
            }
        }
        Ok(missed)
    }

    /// Record the start of one dispatch pass. The row is finished by
    /// [`Self::finish_wake`] or [`Self::fail_wake`]; an unfinished row is the
    /// honest record of a driver that started and never reached a verdict.
    pub async fn begin_wake(&self, source: WakeSource) -> Result<i64, ScheduleError> {
        let now = leveler_core::now();
        let result = sqlx::query("INSERT INTO schedule_wakeups (source, started_at) VALUES (?, ?)")
            .bind(source.as_str())
            .bind(timestamp(now))
            .execute(&self.pool)
            .await?;
        sqlx::query("DELETE FROM schedule_wakeups WHERE started_at < ?")
            .bind(timestamp(now - chrono::Duration::days(WAKE_RETENTION_DAYS)))
            .execute(&self.pool)
            .await?;
        Ok(result.last_insert_rowid())
    }

    /// Finish a wake record with the pass's observed counts.
    pub async fn finish_wake(
        &self,
        id: i64,
        summary: &DispatchSummary,
    ) -> Result<(), ScheduleError> {
        sqlx::query(
            "UPDATE schedule_wakeups SET finished_at = ?, recovered = ?, skipped = ?, \
             missed = ?, claimed = ?, succeeded = ?, submitted = ?, failed = ?, unknown = ?, \
             handed_off = ?, cancelled = ?, waiting_approval = ?, blocked = ?, \
             error = NULL WHERE id = ?",
        )
        .bind(timestamp(leveler_core::now()))
        .bind(summary.recovered as i64)
        .bind(summary.skipped as i64)
        .bind(summary.missed as i64)
        .bind(summary.claimed as i64)
        .bind(summary.succeeded as i64)
        .bind(summary.submitted as i64)
        .bind(summary.failed as i64)
        .bind(summary.unknown as i64)
        .bind(summary.handed_off as i64)
        .bind(summary.cancelled as i64)
        .bind(summary.waiting_approval as i64)
        .bind(summary.blocked as i64)
        .bind(id)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Finish a wake record with a failure. The counts stay at their defaults
    /// and the error names what prevented a verdict.
    pub async fn fail_wake(&self, id: i64, error: &str) -> Result<(), ScheduleError> {
        sqlx::query("UPDATE schedule_wakeups SET finished_at = ?, error = ? WHERE id = ?")
            .bind(timestamp(leveler_core::now()))
            .bind(error)
            .bind(id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    /// Dispatch-pass history, newest first.
    pub async fn wakes(&self, limit: usize) -> Result<Vec<WakeRecord>, ScheduleError> {
        let rows = sqlx::query(
            "SELECT id, source, started_at, finished_at, recovered, skipped, missed, claimed, \
             succeeded, submitted, failed, unknown, handed_off, cancelled, waiting_approval, \
             blocked, error FROM schedule_wakeups \
             ORDER BY started_at DESC, id DESC LIMIT ?",
        )
        .bind(limit as i64)
        .fetch_all(&self.pool)
        .await?;
        rows.iter().map(wake_from_row).collect()
    }

    /// The most recent OS-started dispatch pass, if any.
    pub async fn last_os_wake(&self) -> Result<Option<WakeRecord>, ScheduleError> {
        let row = sqlx::query(
            "SELECT id, source, started_at, finished_at, recovered, skipped, missed, claimed, \
             succeeded, submitted, failed, unknown, handed_off, cancelled, waiting_approval, \
             blocked, error FROM schedule_wakeups \
             WHERE source = 'os' ORDER BY started_at DESC, id DESC LIMIT 1",
        )
        .fetch_optional(&self.pool)
        .await?;
        row.as_ref().map(wake_from_row).transpose()
    }

    /// Pending slots whose time has already passed. Used by diagnostics to
    /// distinguish "nothing due" from "due and nobody woke the dispatcher".
    pub async fn overdue_pending(&self, now: DateTime<Utc>) -> Result<usize, ScheduleError> {
        let count: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM schedule_occurrences o \
             JOIN schedule_definitions d ON d.id = o.definition_id \
             WHERE o.state = 'pending' AND o.scheduled_for <= ? \
               AND (d.enabled = 1 OR o.run_now = 1)",
        )
        .bind(timestamp(now))
        .fetch_one(&self.pool)
        .await?;
        Ok(count.max(0) as usize)
    }

    /// Record that the runtime acknowledged the command envelope. `false`
    /// means the run was already settled (or never existed); the caller must
    /// not treat the delivery as its own.
    pub async fn mark_delivered(&self, run_id: &RunId) -> Result<bool, ScheduleError> {
        let result = sqlx::query(
            "UPDATE schedule_runs SET delivered_at = ? \
             WHERE id = ? AND state = 'dispatching' AND delivered_at IS NULL",
        )
        .bind(timestamp(leveler_core::now()))
        .bind(run_id.as_str())
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected() == 1)
    }

    /// Record that the runtime started the exact command this run delivered.
    pub async fn mark_running(
        &self,
        run_id: &RunId,
        execution_id: &str,
    ) -> Result<bool, ScheduleError> {
        let result = sqlx::query(
            "UPDATE schedule_runs SET state = 'running', execution_id = ?, running_at = ? \
             WHERE id = ? AND state = 'dispatching'",
        )
        .bind(execution_id)
        .bind(timestamp(leveler_core::now()))
        .bind(run_id.as_str())
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected() == 1)
    }

    /// Extend a claim. `false` means the occurrence is no longer claimed here
    /// (recovered by another dispatcher); the caller must stop treating its
    /// wait as authoritative.
    pub async fn renew_lease(
        &self,
        occurrence_id: &OccurrenceId,
        now: DateTime<Utc>,
        lease: Duration,
    ) -> Result<bool, ScheduleError> {
        let result = sqlx::query(
            "UPDATE schedule_occurrences SET lease_expires_at = ? \
             WHERE id = ? AND state = 'claimed'",
        )
        .bind(timestamp(lease_until(now, lease)))
        .bind(occurrence_id.as_str())
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected() == 1)
    }

    /// Record that this dispatcher handed an agent run to the runtime.
    ///
    /// From this point the runtime owns the turn; the schedule owns only the
    /// observation of its terminal record. The claim lease is cleared so
    /// lease recovery cannot misclassify a long, healthy run as a lost
    /// dispatcher. The session baseline and usage baseline make the eventual
    /// settlement attributable to this run and no earlier turn.
    pub async fn mark_handed_off(
        &self,
        run_id: &RunId,
        baseline_sequence: i64,
        turn_ordinal: Option<i64>,
        usage_baseline: Option<&UsageSnapshot>,
        deadline: DateTime<Utc>,
    ) -> Result<bool, ScheduleError> {
        let now = leveler_core::now();
        let mut tx = self.pool.begin().await?;
        let updated = sqlx::query(
            "UPDATE schedule_runs SET state = 'running', \
             delivered_at = COALESCE(delivered_at, ?), \
             running_at = COALESCE(running_at, ?), handed_off_at = ?, deadline_at = ?, \
             baseline_sequence = ?, turn_ordinal = ?, task_sequence = NULL, task_outcome = NULL, \
             usage_baseline_requests = ?, usage_baseline_input_tokens = ?, \
             usage_baseline_cached_input_tokens = ?, usage_baseline_output_tokens = ?, \
             usage_baseline_reasoning_tokens = ?, usage_baseline_cost_usd_micros = ?, \
             usage_baseline_rows_without_cost = ? \
             WHERE id = ? AND state = 'dispatching'",
        )
        .bind(timestamp(now))
        .bind(timestamp(now))
        .bind(timestamp(now))
        .bind(timestamp(deadline))
        .bind(baseline_sequence)
        .bind(turn_ordinal)
        .bind(usage_baseline.map(|usage| usage.requests as i64))
        .bind(usage_baseline.map(|usage| usage.input_tokens as i64))
        .bind(usage_baseline.map(|usage| usage.cached_input_tokens as i64))
        .bind(usage_baseline.map(|usage| usage.output_tokens as i64))
        .bind(usage_baseline.map(|usage| usage.reasoning_tokens as i64))
        .bind(usage_baseline.map(|usage| usage.cost_usd_micros as i64))
        .bind(usage_baseline.map(|usage| usage.rows_without_cost as i64))
        .bind(run_id.as_str())
        .execute(&mut *tx)
        .await?;
        if updated.rows_affected() == 1 {
            sqlx::query(
                "UPDATE schedule_occurrences SET lease_expires_at = NULL \
                 WHERE state = 'claimed' AND id = \
                 (SELECT occurrence_id FROM schedule_runs WHERE id = ?)",
            )
            .bind(run_id.as_str())
            .execute(&mut *tx)
            .await?;
        }
        tx.commit().await?;
        Ok(updated.rows_affected() == 1)
    }

    /// Agent runs whose turn is owned by the runtime right now.
    pub async fn handed_off_runs(&self) -> Result<Vec<HandedOffRun>, ScheduleError> {
        let rows = sqlx::query(&format!(
            "SELECT {RUN_COLUMNS} FROM {RUN_FROM} \
             WHERE r.state = 'running' AND r.handed_off_at IS NOT NULL \
             ORDER BY r.handed_off_at, r.id"
        ))
        .fetch_all(&self.pool)
        .await?;
        let runs: Vec<ScheduleRun> = rows.iter().map(run_from_row).collect::<Result<_, _>>()?;
        let mut handed_off = Vec::with_capacity(runs.len());
        for run in runs {
            if let Some(definition) = self.definition(&run.definition_id).await? {
                handed_off.push(HandedOffRun { run, definition });
            }
        }
        Ok(handed_off)
    }

    /// Ask the dispatcher to stop one active run. This records intent only;
    /// the process that owns the observation issues the runtime cancellation
    /// and settles the terminal fact.
    pub async fn request_cancel(&self, run_id: &RunId) -> Result<bool, ScheduleError> {
        let result = sqlx::query(
            "UPDATE schedule_runs SET cancel_requested_at = COALESCE(cancel_requested_at, ?) \
             WHERE id = ? AND state IN ('dispatching', 'running')",
        )
        .bind(timestamp(leveler_core::now()))
        .bind(run_id.as_str())
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected() == 1)
    }

    /// Request cancellation of every active run of one definition and return
    /// their ids. Pausing or deleting a schedule is intentionally a different
    /// operation and never calls this.
    pub async fn request_cancel_definition(
        &self,
        definition_id: &ScheduleId,
    ) -> Result<Vec<RunId>, ScheduleError> {
        let rows: Vec<String> = sqlx::query_scalar(
            "SELECT id FROM schedule_runs WHERE definition_id = ? \
             AND state IN ('dispatching', 'running')",
        )
        .bind(definition_id.as_str())
        .fetch_all(&self.pool)
        .await?;
        let mut ids = Vec::with_capacity(rows.len());
        for raw in rows {
            let id = RunId::from(raw);
            if self.request_cancel(&id).await? {
                ids.push(id);
            }
        }
        Ok(ids)
    }

    pub async fn cancel_requested(&self, run_id: &RunId) -> Result<bool, ScheduleError> {
        let requested: Option<Option<String>> =
            sqlx::query_scalar("SELECT cancel_requested_at FROM schedule_runs WHERE id = ?")
                .bind(run_id.as_str())
                .fetch_optional(&self.pool)
                .await?;
        Ok(requested.flatten().is_some())
    }

    /// Record that the cancellation was actually sent to the runtime. The
    /// difference between `cancel_requested_at` and this timestamp is what
    /// distinguishes "user asked" from "runtime was told".
    pub async fn mark_cancel_issued(&self, run_id: &RunId) -> Result<bool, ScheduleError> {
        let result = sqlx::query(
            "UPDATE schedule_runs SET cancel_issued_at = COALESCE(cancel_issued_at, ?) \
             WHERE id = ? AND state IN ('dispatching', 'running')",
        )
        .bind(timestamp(leveler_core::now()))
        .bind(run_id.as_str())
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected() == 1)
    }

    /// Settle the run and its occurrence. Idempotent: a run already settled
    /// (for example by lease recovery) is returned unchanged. `None` means the
    /// definition was removed while the run was in flight.
    pub async fn finish_run(
        &self,
        run_id: &RunId,
        outcome: RunOutcome,
    ) -> Result<Option<ScheduleRun>, ScheduleError> {
        self.finish_run_with_facts(run_id, outcome, RunFacts::default())
            .await
    }

    /// Settle a run together with the facts observed about it. An absent fact
    /// never overwrites an earlier observed value.
    pub async fn finish_run_with_facts(
        &self,
        run_id: &RunId,
        outcome: RunOutcome,
        facts: RunFacts,
    ) -> Result<Option<ScheduleRun>, ScheduleError> {
        let now = leveler_core::now();
        let mut tx = self.pool.begin().await?;
        let updated = sqlx::query(
            "UPDATE schedule_runs SET state = ?, exit_code = ?, error = ?, finished_at = ?, \
             task_sequence = COALESCE(?, task_sequence), \
             task_outcome = COALESCE(?, task_outcome), \
             provider = COALESCE(?, provider), \
             native_outcome = COALESCE(?, native_outcome), \
             capability_state = COALESCE(?, capability_state), \
             permission_state = COALESCE(?, permission_state), \
             usage_requests = COALESCE(?, usage_requests), \
             input_tokens = COALESCE(?, input_tokens), \
             cached_input_tokens = COALESCE(?, cached_input_tokens), \
             output_tokens = COALESCE(?, output_tokens), \
             reasoning_tokens = COALESCE(?, reasoning_tokens), \
             cost_usd_micros = COALESCE(?, cost_usd_micros), \
             usage_rows_without_cost = COALESCE(?, usage_rows_without_cost) \
             WHERE id = ? AND state IN ('dispatching', 'running')",
        )
        .bind(outcome.state().as_str())
        .bind(outcome.exit_code())
        .bind(outcome.error())
        .bind(timestamp(now))
        .bind(facts.task_sequence)
        .bind(facts.task_outcome.as_deref())
        .bind(facts.provider.as_deref())
        .bind(facts.native_outcome.as_deref())
        .bind(facts.capability_state.as_deref())
        .bind(facts.permission_state.as_deref())
        .bind(
            facts
                .usage_delta
                .as_ref()
                .map(|usage| usage.requests as i64),
        )
        .bind(
            facts
                .usage_delta
                .as_ref()
                .map(|usage| usage.input_tokens as i64),
        )
        .bind(
            facts
                .usage_delta
                .as_ref()
                .map(|usage| usage.cached_input_tokens as i64),
        )
        .bind(
            facts
                .usage_delta
                .as_ref()
                .map(|usage| usage.output_tokens as i64),
        )
        .bind(
            facts
                .usage_delta
                .as_ref()
                .map(|usage| usage.reasoning_tokens as i64),
        )
        .bind(
            facts
                .usage_delta
                .as_ref()
                .map(|usage| usage.cost_usd_micros as i64),
        )
        .bind(
            facts
                .usage_delta
                .as_ref()
                .map(|usage| usage.rows_without_cost as i64),
        )
        .bind(run_id.as_str())
        .execute(&mut *tx)
        .await?;
        if updated.rows_affected() == 1 {
            sqlx::query(
                "UPDATE schedule_occurrences SET state = ?, lease_expires_at = NULL \
                 WHERE state = 'claimed' AND id = \
                 (SELECT occurrence_id FROM schedule_runs WHERE id = ?)",
            )
            .bind(outcome.occurrence_state().as_str())
            .bind(run_id.as_str())
            .execute(&mut *tx)
            .await?;
        }
        tx.commit().await?;

        let run = self.run(run_id).await?;
        if let Some(run) = &run
            && let Some(definition) = self.definition(&run.definition_id).await?
        {
            self.ensure_next_occurrence(&definition, now).await?;
        }
        Ok(run)
    }

    /// Runs, newest first, optionally narrowed to one definition.
    pub async fn runs(
        &self,
        definition_id: Option<&ScheduleId>,
        limit: usize,
    ) -> Result<Vec<ScheduleRun>, ScheduleError> {
        let rows = match definition_id {
            Some(id) => {
                sqlx::query(&format!(
                    "SELECT {RUN_COLUMNS} FROM {RUN_FROM} \
                     WHERE r.definition_id = ? ORDER BY r.started_at DESC, r.id DESC LIMIT ?"
                ))
                .bind(id.as_str())
                .bind(limit as i64)
                .fetch_all(&self.pool)
                .await?
            }
            None => {
                sqlx::query(&format!(
                    "SELECT {RUN_COLUMNS} FROM {RUN_FROM} \
                     ORDER BY r.started_at DESC, r.id DESC LIMIT ?"
                ))
                .bind(limit as i64)
                .fetch_all(&self.pool)
                .await?
            }
        };
        rows.iter().map(run_from_row).collect()
    }

    /// The earliest pending occurrence across enabled definitions, plus
    /// explicit run-now slots of paused definitions.
    pub async fn next_due(&self) -> Result<Option<DateTime<Utc>>, ScheduleError> {
        let raw: Option<String> = sqlx::query_scalar(
            "SELECT MIN(o.scheduled_for) FROM schedule_occurrences o \
             JOIN schedule_definitions d ON d.id = o.definition_id \
             WHERE o.state = 'pending' AND (d.enabled = 1 OR o.run_now = 1)",
        )
        .fetch_one(&self.pool)
        .await?;
        raw.as_deref().map(parse_timestamp).transpose()
    }

    /// Active (dispatching or running) attempts of one definition.
    pub async fn active_run_count(
        &self,
        definition_id: &ScheduleId,
    ) -> Result<usize, ScheduleError> {
        let count: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM schedule_runs WHERE definition_id = ? \
             AND state IN ('dispatching', 'running')",
        )
        .bind(definition_id.as_str())
        .fetch_one(&self.pool)
        .await?;
        Ok(count as usize)
    }

    /// Active attempt counts for every definition in one read, so a list view
    /// does not issue one query per row.
    pub async fn active_run_counts(&self) -> Result<HashMap<ScheduleId, usize>, ScheduleError> {
        let rows: Vec<(String, i64)> = sqlx::query_as(
            "SELECT definition_id, COUNT(*) FROM schedule_runs \
             WHERE state IN ('dispatching', 'running') GROUP BY definition_id",
        )
        .fetch_all(&self.pool)
        .await?;
        Ok(rows
            .into_iter()
            .map(|(id, count)| (ScheduleId::from(id), count as usize))
            .collect())
    }

    /// The newest attempt of every definition in one read.
    pub async fn latest_run_per_definition(&self) -> Result<Vec<ScheduleRun>, ScheduleError> {
        let rows = sqlx::query(&format!(
            "SELECT {RUN_COLUMNS} FROM {RUN_FROM} \
             JOIN ( \
               SELECT id, ROW_NUMBER() OVER ( \
                 PARTITION BY definition_id ORDER BY started_at DESC, id DESC \
               ) AS rank FROM schedule_runs \
             ) latest ON latest.id = r.id AND latest.rank = 1"
        ))
        .fetch_all(&self.pool)
        .await?;
        rows.iter().map(run_from_row).collect()
    }

    /// The earliest claimable pending slot of every definition in one read.
    pub async fn next_pending_occurrences(&self) -> Result<Vec<Occurrence>, ScheduleError> {
        let rows = sqlx::query(
            "SELECT o.id, o.definition_id, o.scheduled_for, o.state, o.run_now \
             FROM schedule_occurrences o \
             JOIN schedule_definitions d ON d.id = o.definition_id \
             WHERE o.state = 'pending' AND (d.enabled = 1 OR o.run_now = 1) \
             ORDER BY o.scheduled_for ASC, o.id ASC",
        )
        .fetch_all(&self.pool)
        .await?;
        let mut seen = std::collections::HashSet::new();
        let mut next = Vec::new();
        for row in rows.iter() {
            let occurrence = occurrence_from_row(row)?;
            if seen.insert(occurrence.definition_id.clone()) {
                next.push(occurrence);
            }
        }
        Ok(next)
    }

    /// The earliest slot a dispatcher could actually claim: generated slots of
    /// a paused definition are not included, explicit run-now slots are.
    pub async fn next_pending_occurrence(
        &self,
        definition_id: &ScheduleId,
    ) -> Result<Option<Occurrence>, ScheduleError> {
        let row = sqlx::query(
            "SELECT o.id, o.definition_id, o.scheduled_for, o.state, o.run_now \
             FROM schedule_occurrences o \
             JOIN schedule_definitions d ON d.id = o.definition_id \
             WHERE o.definition_id = ? AND o.state = 'pending' \
               AND (d.enabled = 1 OR o.run_now = 1) \
             ORDER BY o.scheduled_for ASC, o.id ASC LIMIT 1",
        )
        .bind(definition_id.as_str())
        .fetch_optional(&self.pool)
        .await?;
        row.as_ref().map(occurrence_from_row).transpose()
    }

    /// Occurrences, newest first, for display and verification.
    pub async fn occurrences(
        &self,
        definition_id: &ScheduleId,
        limit: usize,
    ) -> Result<Vec<Occurrence>, ScheduleError> {
        let rows = sqlx::query(
            "SELECT id, definition_id, scheduled_for, state, run_now FROM schedule_occurrences \
             WHERE definition_id = ? ORDER BY scheduled_for DESC LIMIT ?",
        )
        .bind(definition_id.as_str())
        .bind(limit as i64)
        .fetch_all(&self.pool)
        .await?;
        rows.iter().map(occurrence_from_row).collect()
    }

    async fn occurrence(&self, id: &OccurrenceId) -> Result<Option<Occurrence>, ScheduleError> {
        let row = sqlx::query(
            "SELECT id, definition_id, scheduled_for, state, run_now \
             FROM schedule_occurrences WHERE id = ?",
        )
        .bind(id.as_str())
        .fetch_optional(&self.pool)
        .await?;
        row.as_ref().map(occurrence_from_row).transpose()
    }

    /// One run by id.
    pub async fn run(&self, id: &RunId) -> Result<Option<ScheduleRun>, ScheduleError> {
        let row = sqlx::query(&format!(
            "SELECT {RUN_COLUMNS} FROM {RUN_FROM} WHERE r.id = ?"
        ))
        .bind(id.as_str())
        .fetch_optional(&self.pool)
        .await?;
        row.as_ref().map(run_from_row).transpose()
    }

    /// Create the next occurrence from the rule's own grid.
    ///
    /// The successor is `next_after` the latest materialized slot, not
    /// `next_after` the moment the previous run happened to finish. A slot
    /// that is already in the future is left alone, so a pass cannot stack
    /// a second future slot. `queue-one` keeps at most one pending demand
    /// while an earlier one is still waiting. A successor older than the
    /// misfire grace is not backfilled: the dispatcher was absent, and the
    /// misfire policy owns that gap rather than a burst of catch-up slots.
    /// The unique `(definition, scheduled_for)` key makes a double call safe.
    pub async fn ensure_next_occurrence(
        &self,
        definition: &ScheduleDefinition,
        now: DateTime<Utc>,
    ) -> Result<Option<Occurrence>, ScheduleError> {
        if !definition.enabled {
            return Ok(None);
        }
        if definition.overlap == OverlapPolicy::QueueOne {
            let pending: i64 = sqlx::query_scalar(
                "SELECT COUNT(*) FROM schedule_occurrences \
                 WHERE definition_id = ? AND state = 'pending'",
            )
            .bind(definition.id.as_str())
            .fetch_one(&self.pool)
            .await?;
            if pending > 0 {
                return Ok(None);
            }
        }
        let last: Option<String> = sqlx::query_scalar(
            "SELECT MAX(scheduled_for) FROM schedule_occurrences WHERE definition_id = ?",
        )
        .bind(definition.id.as_str())
        .fetch_one(&self.pool)
        .await?;
        let last = last.as_deref().map(parse_timestamp).transpose()?;
        if let Some(last) = last
            && last > now
        {
            return Ok(None);
        }
        let base = last.unwrap_or(now);
        let Some(mut next) = definition.trigger.next_after(base)? else {
            return Ok(None);
        };
        let grace = chrono::Duration::from_std(definition.misfire_grace)
            .unwrap_or_else(|_| chrono::Duration::zero());
        if next <= now && now.signed_duration_since(next) > grace {
            let Some(live) = definition.trigger.next_after(now)? else {
                return Ok(None);
            };
            next = live;
        }
        if let Some(last) = last
            && next <= last
        {
            return Ok(None);
        }
        let id = OccurrenceId::generate();
        sqlx::query(
            "INSERT OR IGNORE INTO schedule_occurrences \
             (id, definition_id, scheduled_for, state, created_at) VALUES (?, ?, ?, 'pending', ?)",
        )
        .bind(id.as_str())
        .bind(definition.id.as_str())
        .bind(timestamp(next))
        .bind(timestamp(now))
        .execute(&self.pool)
        .await?;
        let row = sqlx::query(
            "SELECT id, definition_id, scheduled_for, state, run_now \
             FROM schedule_occurrences \
             WHERE definition_id = ? AND scheduled_for = ?",
        )
        .bind(definition.id.as_str())
        .bind(timestamp(next))
        .fetch_optional(&self.pool)
        .await?;
        row.as_ref().map(occurrence_from_row).transpose()
    }

    /// Leave a delivered shell for a later dispatch pass to observe.
    ///
    /// The claim lease is cleared so lease recovery cannot mark a live shell
    /// unknown just because this process returned to the OS scheduler. The
    /// deadline is the run's own timeout. Nothing here delivers the command
    /// again.
    pub async fn release_shell_observation(
        &self,
        run_id: &RunId,
        deadline: DateTime<Utc>,
    ) -> Result<bool, ScheduleError> {
        let mut tx = self.pool.begin().await?;
        let updated = sqlx::query(
            "UPDATE schedule_runs SET deadline_at = ? \
             WHERE id = ? AND state IN ('dispatching', 'running') AND delivered_at IS NOT NULL",
        )
        .bind(timestamp(deadline))
        .bind(run_id.as_str())
        .execute(&mut *tx)
        .await?;
        if updated.rows_affected() == 1 {
            sqlx::query(
                "UPDATE schedule_occurrences SET lease_expires_at = NULL \
                 WHERE state = 'claimed' AND id = \
                 (SELECT occurrence_id FROM schedule_runs WHERE id = ?)",
            )
            .bind(run_id.as_str())
            .execute(&mut *tx)
            .await?;
        }
        tx.commit().await?;
        Ok(updated.rows_affected() == 1)
    }

    /// Delivered shell runs whose terminal record is still unread.
    ///
    /// Agent handoffs are excluded: they carry `handed_off_at` and are
    /// observed by the agent reconciler. A shell row here was accepted by
    /// the runtime and must not be delivered again.
    pub async fn observable_shell_runs(&self) -> Result<Vec<HandedOffRun>, ScheduleError> {
        let rows = sqlx::query(&format!(
            "SELECT {RUN_COLUMNS} FROM {RUN_FROM} \
             JOIN schedule_definitions d ON d.id = r.definition_id \
             WHERE d.action = 'shell' AND r.state IN ('dispatching', 'running') \
               AND r.delivered_at IS NOT NULL AND r.deadline_at IS NOT NULL \
               AND r.handed_off_at IS NULL \
             ORDER BY r.delivered_at, r.id"
        ))
        .fetch_all(&self.pool)
        .await?;
        let runs: Vec<ScheduleRun> = rows.iter().map(run_from_row).collect::<Result<_, _>>()?;
        let mut observing = Vec::with_capacity(runs.len());
        for run in runs {
            if let Some(definition) = self.definition(&run.definition_id).await? {
                observing.push(HandedOffRun { run, definition });
            }
        }
        Ok(observing)
    }
}

fn storage_time(row: &sqlx::sqlite::SqliteRow) -> Result<crate::LocalTime, ScheduleError> {
    let raw: String = row.try_get("local_time")?;
    crate::LocalTime::parse(&raw)
}

fn storage_timezone(row: &sqlx::sqlite::SqliteRow) -> Result<chrono_tz::Tz, ScheduleError> {
    let raw: String = row.try_get("timezone")?;
    crate::parse_timezone(&raw)
}

struct ActionColumns {
    kind: &'static str,
    agent_goal: Option<String>,
    native_action: Option<String>,
    native_input: Option<String>,
}

/// A grant must name every scope its action can reach. A shell command can
/// write anywhere the user can and an agent's tools can act outside the
/// workspace, so both need an explicit external-side-effect grant. A native
/// action is classified by the executor at dispatch time, which checks the
/// grant against the observed classification.
fn validate_unattended_request(
    action: &ScheduleAction,
    request: UnattendedRequest,
) -> Result<(), ScheduleError> {
    let always_external = matches!(
        action,
        ScheduleAction::Shell { .. } | ScheduleAction::Agent { .. }
    );
    if always_external && !request.allow_external_side_effects {
        return Err(ScheduleError::Invalid(format!(
            "a scheduled {} action can create effects outside the workspace; the grant \
             must explicitly allow external side effects",
            action.kind().as_str()
        )));
    }
    Ok(())
}

/// The one encoding of a typed action into its columns, shared by create and
/// update so the two writers cannot drift.
fn action_columns(action: &ScheduleAction) -> Result<ActionColumns, ScheduleError> {
    let (agent_goal, native_action, native_input) = match action {
        ScheduleAction::Native { action, input } => (
            None,
            Some(action.clone()),
            Some(serde_json::to_string(input).map_err(|error| {
                ScheduleError::Invalid(format!(
                    "the native action input is not serializable: {error}"
                ))
            })?),
        ),
        ScheduleAction::Agent { goal } => (Some(goal.clone()), None, None),
        ScheduleAction::Shell { .. } => (None, None, None),
    };
    Ok(ActionColumns {
        kind: action.kind().as_str(),
        agent_goal,
        native_action,
        native_input,
    })
}

struct TriggerColumns {
    kind: &'static str,
    once_at: Option<String>,
    anchor_at: Option<String>,
    every_secs: Option<i64>,
    timezone: Option<String>,
    local_time: Option<String>,
    weekdays: Option<i64>,
    cron_expr: Option<String>,
}

/// The one encoding of a trigger into its columns, shared by create and update.
fn trigger_columns(trigger: &ScheduleTrigger) -> TriggerColumns {
    match trigger {
        ScheduleTrigger::Once { at } => TriggerColumns {
            kind: "once",
            once_at: Some(timestamp(*at)),
            anchor_at: None,
            every_secs: None,
            timezone: None,
            local_time: None,
            weekdays: None,
            cron_expr: None,
        },
        ScheduleTrigger::Every { anchor, every } => TriggerColumns {
            kind: "every",
            once_at: None,
            anchor_at: Some(timestamp(*anchor)),
            every_secs: Some(every.as_secs() as i64),
            timezone: None,
            local_time: None,
            weekdays: None,
            cron_expr: None,
        },
        ScheduleTrigger::Daily { time, timezone } => TriggerColumns {
            kind: "daily",
            once_at: None,
            anchor_at: None,
            every_secs: None,
            timezone: Some(timezone.to_string()),
            local_time: Some(time.to_string()),
            weekdays: None,
            cron_expr: None,
        },
        ScheduleTrigger::Weekly {
            weekdays,
            time,
            timezone,
        } => TriggerColumns {
            kind: "weekly",
            once_at: None,
            anchor_at: None,
            every_secs: None,
            timezone: Some(timezone.to_string()),
            local_time: Some(time.to_string()),
            weekdays: Some(i64::from(weekdays.bits())),
            cron_expr: None,
        },
        ScheduleTrigger::Cron {
            expression,
            timezone,
        } => TriggerColumns {
            kind: "cron",
            once_at: None,
            anchor_at: None,
            every_secs: None,
            timezone: Some(timezone.to_string()),
            local_time: None,
            weekdays: None,
            cron_expr: Some(expression.clone()),
        },
    }
}

fn definition_from_row(row: &sqlx::sqlite::SqliteRow) -> Result<ScheduleDefinition, ScheduleError> {
    let action_kind: String = row.try_get("action")?;
    let action = match action_kind.as_str() {
        "shell" => ScheduleAction::Shell {
            command: row.try_get("command")?,
        },
        "native" => {
            let id: String = row.try_get("native_action")?;
            let input: String = row.try_get("native_input")?;
            ScheduleAction::Native {
                action: id,
                input: serde_json::from_str(&input).map_err(|error| {
                    ScheduleError::Invalid(format!(
                        "stored native action input is not valid JSON: {error}"
                    ))
                })?,
            }
        }
        "agent" => ScheduleAction::Agent {
            goal: row.try_get("agent_goal")?,
        },
        other => {
            return Err(ScheduleError::Invalid(format!(
                "unsupported schedule action `{other}`"
            )));
        }
    };
    action.validate()?;
    let session_policy_raw: String = row.try_get("session_policy")?;
    let session_policy = SessionPolicy::parse(&session_policy_raw).ok_or_else(|| {
        ScheduleError::Invalid(format!("unsupported session policy `{session_policy_raw}`"))
    })?;
    let overlap_raw: String = row.try_get("overlap_policy")?;
    let overlap = OverlapPolicy::parse(&overlap_raw).ok_or_else(|| {
        ScheduleError::Invalid(format!("unsupported overlap policy `{overlap_raw}`"))
    })?;
    let grant_raw: Option<String> = row.try_get("unattended_grant")?;
    let unattended: Option<UnattendedGrant> = grant_raw
        .as_deref()
        .map(|raw| {
            serde_json::from_str(raw).map_err(|error| {
                ScheduleError::Invalid(format!(
                    "stored unattended grant is not valid JSON: {error}"
                ))
            })
        })
        .transpose()?;
    let trigger_kind: String = row.try_get("trigger_kind")?;
    let trigger = match trigger_kind.as_str() {
        "once" => {
            let raw: String = row.try_get("once_at")?;
            ScheduleTrigger::Once {
                at: parse_timestamp(&raw)?,
            }
        }
        "every" => {
            let raw: String = row.try_get("anchor_at")?;
            let every_secs: i64 = row.try_get("every_secs")?;
            ScheduleTrigger::Every {
                anchor: parse_timestamp(&raw)?,
                every: Duration::from_secs(every_secs.max(1) as u64),
            }
        }
        "daily" => ScheduleTrigger::Daily {
            time: storage_time(row)?,
            timezone: storage_timezone(row)?,
        },
        "weekly" => ScheduleTrigger::Weekly {
            weekdays: crate::WeekdaySet::from_bits(
                u8::try_from(row.try_get::<i64, _>("weekdays")?)
                    .map_err(|_| ScheduleError::Invalid("invalid weekday mask".into()))?,
            )?,
            time: storage_time(row)?,
            timezone: storage_timezone(row)?,
        },
        "cron" => ScheduleTrigger::Cron {
            expression: row.try_get::<String, _>("cron_expr")?,
            timezone: storage_timezone(row)?,
        },
        other => {
            return Err(ScheduleError::Invalid(format!(
                "unsupported schedule trigger `{other}`"
            )));
        }
    };
    trigger.validate()?;
    let permission_mode: String = row.try_get("permission_mode")?;
    let mode = PermissionProfile::parse(&permission_mode).ok_or_else(|| {
        ScheduleError::Invalid(format!("unsupported permission mode `{permission_mode}`"))
    })?;
    let model: Option<String> = row.try_get("model")?;
    let model =
        match model {
            Some(raw) => Some(ModelRef::parse(&raw).ok_or_else(|| {
                ScheduleError::Invalid(format!("invalid model reference `{raw}`"))
            })?),
            None => None,
        };
    let timeout_secs: i64 = row.try_get("timeout_secs")?;
    let misfire_policy: String = row.try_get("misfire_policy")?;
    let misfire = MisfirePolicy::parse(&misfire_policy).ok_or_else(|| {
        ScheduleError::Invalid(format!("unsupported misfire policy `{misfire_policy}`"))
    })?;
    let misfire_grace_secs: i64 = row.try_get("misfire_grace_secs")?;
    let session: Option<String> = row.try_get("session_id")?;
    let created_at: String = row.try_get("created_at")?;
    let updated_at: String = row.try_get("updated_at")?;
    Ok(ScheduleDefinition {
        id: ScheduleId::from(row.try_get::<String, _>("id")?),
        name: row.try_get("name")?,
        workspace: std::path::PathBuf::from(row.try_get::<String, _>("workspace")?),
        action,
        trigger,
        session_policy,
        overlap,
        mode,
        model,
        timeout: Duration::from_secs(timeout_secs.max(1) as u64),
        misfire,
        misfire_grace: Duration::from_secs(misfire_grace_secs.max(0) as u64),
        session_id: session.map(SessionId::from),
        unattended,
        enabled: row.try_get::<i64, _>("enabled")? != 0,
        created_at: parse_timestamp(&created_at)?,
        updated_at: parse_timestamp(&updated_at)?,
    })
}

fn occurrence_from_row(row: &sqlx::sqlite::SqliteRow) -> Result<Occurrence, ScheduleError> {
    let state: String = row.try_get("state")?;
    Ok(Occurrence {
        id: OccurrenceId::from(row.try_get::<String, _>("id")?),
        definition_id: ScheduleId::from(row.try_get::<String, _>("definition_id")?),
        scheduled_for: parse_timestamp(&row.try_get::<String, _>("scheduled_for")?)?,
        state: OccurrenceState::parse(&state).ok_or_else(|| {
            ScheduleError::Invalid(format!("unsupported occurrence state `{state}`"))
        })?,
        run_now: row.try_get::<i64, _>("run_now")? != 0,
    })
}

fn run_from_row(row: &sqlx::sqlite::SqliteRow) -> Result<ScheduleRun, ScheduleError> {
    let state: String = row.try_get("state")?;
    let session: Option<String> = row.try_get("session_id")?;
    Ok(ScheduleRun {
        id: RunId::from(row.try_get::<String, _>("id")?),
        occurrence_id: OccurrenceId::from(row.try_get::<String, _>("occurrence_id")?),
        definition_id: ScheduleId::from(row.try_get::<String, _>("definition_id")?),
        scheduled_for: parse_timestamp(&row.try_get::<String, _>("scheduled_for")?)?,
        state: RunState::parse(&state)
            .ok_or_else(|| ScheduleError::Invalid(format!("unsupported run state `{state}`")))?,
        session_id: session.map(SessionId::from),
        execution_id: row.try_get("execution_id")?,
        exit_code: row.try_get("exit_code")?,
        error: row.try_get("error")?,
        started_at: parse_timestamp(&row.try_get::<String, _>("started_at")?)?,
        delivered_at: optional_timestamp(row, "delivered_at")?,
        running_at: optional_timestamp(row, "running_at")?,
        finished_at: optional_timestamp(row, "finished_at")?,
        baseline_sequence: row.try_get("baseline_sequence")?,
        turn_ordinal: row.try_get("turn_ordinal")?,
        task_sequence: row.try_get("task_sequence")?,
        task_outcome: row.try_get("task_outcome")?,
        provider: row.try_get("provider")?,
        native_outcome: row.try_get("native_outcome")?,
        capability_state: row.try_get("capability_state")?,
        permission_state: row.try_get("permission_state")?,
        usage_requests: optional_u64(row, "usage_requests")?,
        input_tokens: optional_u64(row, "input_tokens")?,
        cached_input_tokens: optional_u64(row, "cached_input_tokens")?,
        output_tokens: optional_u64(row, "output_tokens")?,
        reasoning_tokens: optional_u64(row, "reasoning_tokens")?,
        cost_usd_micros: optional_u64(row, "cost_usd_micros")?,
        usage_rows_without_cost: optional_u64(row, "usage_rows_without_cost")?,
        usage_baseline: usage_baseline_from_row(row)?,
        handed_off_at: optional_timestamp(row, "handed_off_at")?,
        deadline_at: optional_timestamp(row, "deadline_at")?,
        cancel_requested_at: optional_timestamp(row, "cancel_requested_at")?,
        cancel_issued_at: optional_timestamp(row, "cancel_issued_at")?,
    })
}

fn optional_u64(row: &sqlx::sqlite::SqliteRow, column: &str) -> Result<Option<u64>, ScheduleError> {
    let raw: Option<i64> = row.try_get(column)?;
    Ok(raw.map(|value| value.max(0) as u64))
}

/// A baseline is all-or-nothing: the writer stores every component together.
fn usage_baseline_from_row(
    row: &sqlx::sqlite::SqliteRow,
) -> Result<Option<UsageSnapshot>, ScheduleError> {
    if optional_u64(row, "usage_baseline_requests")?.is_none() {
        return Ok(None);
    }
    Ok(Some(UsageSnapshot {
        requests: optional_u64(row, "usage_baseline_requests")?.unwrap_or_default(),
        input_tokens: optional_u64(row, "usage_baseline_input_tokens")?.unwrap_or_default(),
        cached_input_tokens: optional_u64(row, "usage_baseline_cached_input_tokens")?
            .unwrap_or_default(),
        output_tokens: optional_u64(row, "usage_baseline_output_tokens")?.unwrap_or_default(),
        reasoning_tokens: optional_u64(row, "usage_baseline_reasoning_tokens")?.unwrap_or_default(),
        cost_usd_micros: optional_u64(row, "usage_baseline_cost_usd_micros")?.unwrap_or_default(),
        rows_without_cost: optional_u64(row, "usage_baseline_rows_without_cost")?
            .unwrap_or_default(),
    }))
}

fn wake_from_row(row: &sqlx::sqlite::SqliteRow) -> Result<WakeRecord, ScheduleError> {
    let source: String = row.try_get("source")?;
    Ok(WakeRecord {
        id: row.try_get("id")?,
        source: WakeSource::parse(&source)
            .ok_or_else(|| ScheduleError::Invalid(format!("unsupported wake source `{source}`")))?,
        started_at: parse_timestamp(&row.try_get::<String, _>("started_at")?)?,
        finished_at: optional_timestamp(row, "finished_at")?,
        recovered: usize_of(row, "recovered")?,
        skipped: usize_of(row, "skipped")?,
        missed: usize_of(row, "missed")?,
        claimed: usize_of(row, "claimed")?,
        succeeded: usize_of(row, "succeeded")?,
        submitted: usize_of(row, "submitted")?,
        failed: usize_of(row, "failed")?,
        unknown: usize_of(row, "unknown")?,
        handed_off: usize_of(row, "handed_off")?,
        cancelled: usize_of(row, "cancelled")?,
        waiting_approval: usize_of(row, "waiting_approval")?,
        blocked: usize_of(row, "blocked")?,
        error: row.try_get("error")?,
    })
}

fn optional_timestamp(
    row: &sqlx::sqlite::SqliteRow,
    column: &str,
) -> Result<Option<DateTime<Utc>>, ScheduleError> {
    row.try_get::<Option<String>, _>(column)?
        .as_deref()
        .map(parse_timestamp)
        .transpose()
}

fn usize_of(row: &sqlx::sqlite::SqliteRow, column: &str) -> Result<usize, ScheduleError> {
    let value: i64 = row.try_get(column)?;
    Ok(value.max(0) as usize)
}

fn lease_until(now: DateTime<Utc>, lease: Duration) -> DateTime<Utc> {
    now + chrono::Duration::seconds(lease.as_secs().min(i64::MAX as u64) as i64)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{NewScheduleDefinition, ScheduleTrigger};

    async fn store() -> ScheduleStore {
        ScheduleStore::open_in_memory().await.unwrap()
    }

    #[tokio::test]
    async fn calendar_triggers_round_trip_and_schedule_from_the_rule() {
        let store = store().await;
        let daily = ScheduleTrigger::Daily {
            time: crate::LocalTime::parse("09:00").unwrap(),
            timezone: crate::parse_timezone("Asia/Shanghai").unwrap(),
        };
        let created = store.create_definition(definition(daily)).await.unwrap();
        let reread = store.definition(&created.id).await.unwrap().unwrap();
        assert_eq!(reread.trigger, created.trigger);

        let first = store.occurrences(&created.id, 10).await.unwrap();
        assert_eq!(first.len(), 1);
        assert!(first[0].scheduled_for > leveler_core::now());

        // Three hours after the first slot, the next one is still the next
        // local 09:00 - not "three hours after the run finished".
        let next = store
            .ensure_next_occurrence(&reread, first[0].scheduled_for + chrono::Duration::hours(3))
            .await
            .unwrap()
            .expect("next daily occurrence");
        assert_eq!(
            next.scheduled_for,
            first[0].scheduled_for + chrono::Duration::days(1)
        );
    }

    #[tokio::test]
    async fn a_late_ensure_keeps_the_interval_grid_while_a_run_is_active() {
        let store = store().await;
        let anchor = leveler_core::now() - chrono::Duration::seconds(1);
        let created = store
            .create_definition(grid_definition(
                anchor,
                Duration::from_secs(60),
                OverlapPolicy::AllowConcurrent,
                SessionPolicy::Fresh,
            ))
            .await
            .unwrap();
        let claimed = store
            .claim_due(leveler_core::now(), Duration::from_secs(30), 4)
            .await
            .unwrap();
        assert_eq!(claimed.len(), 1);
        let late = anchor + chrono::Duration::seconds(150);
        let next = store
            .ensure_next_occurrence(&created, late)
            .await
            .unwrap()
            .expect("the grid successor");
        assert_eq!(
            next.scheduled_for,
            at_storage_precision(anchor + chrono::Duration::seconds(60)),
            "the next slot follows the rule, not the previous run's finish time"
        );
    }

    #[tokio::test]
    async fn queue_one_keeps_a_single_pending_demand_while_a_run_is_active() {
        let store = store().await;
        let anchor = leveler_core::now() - chrono::Duration::seconds(1);
        let created = store
            .create_definition(grid_definition(
                anchor,
                Duration::from_secs(60),
                OverlapPolicy::QueueOne,
                SessionPolicy::Continue,
            ))
            .await
            .unwrap();
        let claimed = store
            .claim_due(leveler_core::now(), Duration::from_secs(30), 4)
            .await
            .unwrap();
        assert_eq!(claimed.len(), 1);
        let queued = store
            .ensure_next_occurrence(&created, anchor + chrono::Duration::seconds(1))
            .await
            .unwrap()
            .expect("one queued slot");
        assert_eq!(
            queued.scheduled_for,
            at_storage_precision(anchor + chrono::Duration::seconds(60))
        );
        let again = store
            .ensure_next_occurrence(&created, anchor + chrono::Duration::seconds(150))
            .await
            .unwrap();
        assert!(again.is_none(), "queue-one must not accumulate: {again:?}");
        let pending = store
            .occurrences(&created.id, 10)
            .await
            .unwrap()
            .into_iter()
            .filter(|occurrence| occurrence.state == OccurrenceState::Pending)
            .count();
        assert_eq!(pending, 1);
    }

    #[tokio::test]
    async fn skip_if_running_records_a_due_slot_that_was_materialized_during_the_run() {
        let store = store().await;
        let anchor = leveler_core::now() - chrono::Duration::seconds(1);
        let created = store
            .create_definition(grid_definition(
                anchor,
                Duration::from_secs(60),
                OverlapPolicy::SkipIfRunning,
                SessionPolicy::Continue,
            ))
            .await
            .unwrap();
        let claimed = store
            .claim_due(leveler_core::now(), Duration::from_secs(120), 4)
            .await
            .unwrap();
        assert_eq!(claimed.len(), 1);
        let due = store
            .ensure_next_occurrence(&created, anchor + chrono::Duration::seconds(1))
            .await
            .unwrap()
            .expect("the next trigger exists before it is due");
        let skipped = store
            .reconcile_overlaps(due.scheduled_for + chrono::Duration::seconds(1))
            .await
            .unwrap();
        assert_eq!(skipped, 1);
        let runs = store.runs(Some(&created.id), 10).await.unwrap();
        assert!(
            runs.iter().any(|run| run.state == RunState::Skipped),
            "a due trigger during an active run is recorded skipped: {runs:?}"
        );
        let pending = store
            .occurrences(&created.id, 10)
            .await
            .unwrap()
            .into_iter()
            .filter(|occurrence| occurrence.state == OccurrenceState::Pending)
            .count();
        assert_eq!(
            pending, 1,
            "the following grid slot replaces the skipped one"
        );
    }

    #[tokio::test]
    async fn weekly_and_cron_definitions_round_trip() {
        let store = store().await;
        let weekly = ScheduleTrigger::Weekly {
            weekdays: crate::WeekdaySet::parse("mon,wed,fri").unwrap(),
            time: crate::LocalTime::parse("07:45").unwrap(),
            timezone: crate::parse_timezone("Europe/Berlin").unwrap(),
        };
        let cron = ScheduleTrigger::Cron {
            expression: "0 9 1 * *".into(),
            timezone: crate::parse_timezone("America/New_York").unwrap(),
        };
        for trigger in [weekly, cron] {
            let created = store.create_definition(definition(trigger)).await.unwrap();
            let reread = store.definition(&created.id).await.unwrap().unwrap();
            assert_eq!(reread.trigger, created.trigger);
        }
    }

    #[tokio::test]
    async fn invalid_calendar_rules_never_reach_storage() {
        let store = store().await;
        let trigger = ScheduleTrigger::Cron {
            expression: "not a cron".into(),
            timezone: crate::parse_timezone("UTC").unwrap(),
        };
        assert!(store.create_definition(definition(trigger)).await.is_err());
        assert!(store.definitions().await.unwrap().is_empty());
    }

    /// Schedule instants round-trip through RFC3339 milliseconds.
    fn at_storage_precision(value: DateTime<Utc>) -> DateTime<Utc> {
        parse_timestamp(&timestamp(value)).expect("storage timestamp")
    }

    fn grid_definition(
        anchor: DateTime<Utc>,
        every: Duration,
        overlap: OverlapPolicy,
        session_policy: SessionPolicy,
    ) -> NewScheduleDefinition {
        let mut definition = definition(ScheduleTrigger::Every { anchor, every });
        definition.overlap = overlap;
        definition.session_policy = session_policy;
        definition.misfire_grace = Duration::from_secs(180);
        definition
    }

    fn definition(trigger: ScheduleTrigger) -> NewScheduleDefinition {
        let workspace = std::env::temp_dir().join("leveler-schedule-test");
        std::fs::create_dir_all(&workspace).unwrap();
        NewScheduleDefinition {
            name: "nightly".into(),
            workspace,
            action: ScheduleAction::Shell {
                command: "printf ok".into(),
            },
            trigger,
            session_policy: SessionPolicy::Continue,
            overlap: OverlapPolicy::SkipIfRunning,
            mode: PermissionProfile::Assisted,
            model: None,
            timeout: Duration::from_secs(60),
            misfire: MisfirePolicy::Skip,
            misfire_grace: Duration::from_secs(300),
            unattended: None,
        }
    }

    fn session(id: &str) -> SessionId {
        SessionId::new(id)
    }

    #[tokio::test]
    async fn a_due_occurrence_is_claimed_once_and_records_one_run() {
        let store = store().await;
        let now = leveler_core::now();
        let created = store
            .create_definition(definition(ScheduleTrigger::Once {
                at: now - chrono::Duration::seconds(1),
            }))
            .await
            .unwrap();
        assert_eq!(store.definitions().await.unwrap().len(), 1);

        let claims = store
            .claim_due(now, Duration::from_secs(60), 10)
            .await
            .unwrap();
        assert_eq!(claims.len(), 1);
        assert_eq!(claims[0].definition.id, created.id);
        assert_eq!(claims[0].occurrence.state, OccurrenceState::Claimed);

        assert!(
            store
                .claim_due(now, Duration::from_secs(60), 10)
                .await
                .unwrap()
                .is_empty(),
            "a claimed occurrence was claimed twice"
        );
        let runs = store.runs(Some(&created.id), 10).await.unwrap();
        assert_eq!(runs.len(), 1);
        assert_eq!(runs[0].state, RunState::Dispatching);
        assert_eq!(runs[0].occurrence_id, claims[0].occurrence.id);
    }

    #[tokio::test]
    async fn one_pass_claims_at_most_one_due_slot_of_a_skip_if_running_schedule() {
        let store = store().await;
        let now = leveler_core::now();
        let created = store
            .create_definition(definition(ScheduleTrigger::Once {
                at: now - chrono::Duration::seconds(1),
            }))
            .await
            .unwrap();
        // A run-now slot is due immediately, so one pass sees two due slots of
        // the same definition. Claiming both would start two runs at once and
        // bypass the skip-if-running overlap policy.
        store.create_run_now(&created).await.unwrap();

        // The run-now slot is stamped at creation; evaluate the pass just
        // after it so both slots are due.
        let due = leveler_core::now() + chrono::Duration::seconds(1);
        let claims = store
            .claim_due(due, Duration::from_secs(60), 10)
            .await
            .unwrap();

        assert_eq!(
            claims.len(),
            1,
            "skip-if-running must start at most one run of a definition per pass"
        );
        let pending = store
            .occurrences(&created.id, 10)
            .await
            .unwrap()
            .into_iter()
            .filter(|occurrence| occurrence.state == OccurrenceState::Pending)
            .count();
        assert_eq!(
            pending, 1,
            "the second due slot stays pending for the overlap policy to settle"
        );
    }

    #[tokio::test]
    async fn finishing_a_recurring_run_schedules_one_future_slot() {
        let store = store().await;
        let now = leveler_core::now();
        let created = store
            .create_definition(definition(ScheduleTrigger::Every {
                anchor: now - chrono::Duration::seconds(3600),
                every: Duration::from_secs(60),
            }))
            .await
            .unwrap();
        let claims = store
            .claim_due(now, Duration::from_secs(60), 10)
            .await
            .unwrap();
        assert_eq!(claims.len(), 1);

        let run = store
            .finish_run(
                &claims[0].run_id,
                RunOutcome::Succeeded { exit_code: Some(0) },
            )
            .await
            .unwrap()
            .expect("the run still existed");
        assert_eq!(run.state, RunState::Succeeded);

        let occurrences = store.occurrences(&created.id, 10).await.unwrap();
        assert_eq!(occurrences.len(), 2, "missed slots must not be backfilled");
        assert!(
            occurrences
                .iter()
                .any(|occurrence| occurrence.state == OccurrenceState::Completed)
        );
        let pending: Vec<_> = occurrences
            .iter()
            .filter(|occurrence| occurrence.state == OccurrenceState::Pending)
            .collect();
        assert_eq!(pending.len(), 1);
        assert!(pending[0].scheduled_for > now);
        assert!(pending[0].scheduled_for <= now + chrono::Duration::seconds(61));
    }

    #[tokio::test]
    async fn a_once_run_has_no_successor() {
        let store = store().await;
        let now = leveler_core::now();
        let created = store
            .create_definition(definition(ScheduleTrigger::Once {
                at: now - chrono::Duration::seconds(1),
            }))
            .await
            .unwrap();
        let claims = store
            .claim_due(now, Duration::from_secs(60), 10)
            .await
            .unwrap();
        store
            .finish_run(
                &claims[0].run_id,
                RunOutcome::Succeeded { exit_code: Some(0) },
            )
            .await
            .unwrap();
        let occurrences = store.occurrences(&created.id, 10).await.unwrap();
        assert_eq!(occurrences.len(), 1);
        assert_eq!(occurrences[0].state, OccurrenceState::Completed);
    }

    #[tokio::test]
    async fn an_expired_lease_settles_unknown_and_is_not_retried() {
        let store = store().await;
        let now = leveler_core::now();
        let created = store
            .create_definition(definition(ScheduleTrigger::Every {
                anchor: now,
                every: Duration::from_secs(60),
            }))
            .await
            .unwrap();
        let claims = store
            .claim_due(now, Duration::from_secs(5), 10)
            .await
            .unwrap();
        assert_eq!(claims.len(), 1);
        store
            .mark_session(&claims[0].run_id, &session("s-1"))
            .await
            .unwrap();
        assert!(
            store
                .mark_running(&claims[0].run_id, "ush-1")
                .await
                .unwrap()
        );

        let later = now + chrono::Duration::seconds(6);
        let recovered = store.recover_expired(later).await.unwrap();
        assert_eq!(recovered.len(), 1);
        let run = store.run(&claims[0].run_id).await.unwrap().unwrap();
        assert_eq!(run.state, RunState::Unknown);
        assert_eq!(run.execution_id.as_deref(), Some("ush-1"));
        assert!(run.error.is_some(), "an unknown run must say why");

        let occurrences = store.occurrences(&created.id, 10).await.unwrap();
        assert!(
            occurrences
                .iter()
                .any(|occurrence| occurrence.state == OccurrenceState::Interrupted)
        );
        let pending = occurrences
            .iter()
            .find(|occurrence| occurrence.state == OccurrenceState::Pending)
            .expect("a recurring definition keeps a future slot");
        assert!(pending.scheduled_for > later);
        assert!(
            store
                .claim_due(later, Duration::from_secs(5), 10)
                .await
                .unwrap()
                .is_empty(),
            "an interrupted occurrence was retried"
        );
    }

    #[tokio::test]
    async fn a_definition_session_is_bound_once_and_never_replaced() {
        let store = store().await;
        let now = leveler_core::now();
        let created = store
            .create_definition(definition(ScheduleTrigger::Once { at: now }))
            .await
            .unwrap();
        assert_eq!(
            store
                .set_session(&created.id, &session("s-1"))
                .await
                .unwrap()
                .as_str(),
            "s-1"
        );
        assert_eq!(
            store
                .set_session(&created.id, &session("s-1"))
                .await
                .unwrap()
                .as_str(),
            "s-1"
        );
        let error = store
            .set_session(&created.id, &session("s-2"))
            .await
            .unwrap_err();
        assert!(matches!(error, ScheduleError::SessionMismatch(_)));
        assert_eq!(
            store
                .definition(&created.id)
                .await
                .unwrap()
                .unwrap()
                .session_id
                .unwrap()
                .as_str(),
            "s-1"
        );
    }

    #[tokio::test]
    async fn removing_a_definition_removes_its_history() {
        let store = store().await;
        let now = leveler_core::now();
        let created = store
            .create_definition(definition(ScheduleTrigger::Every {
                anchor: now,
                every: Duration::from_secs(60),
            }))
            .await
            .unwrap();
        let claims = store
            .claim_due(now, Duration::from_secs(60), 10)
            .await
            .unwrap();
        assert_eq!(claims.len(), 1);
        // A definition with a live run is not removed: the runtime's task
        // would be orphaned with no schedule record owning its settlement.
        let refused = store.remove_definition(&created.id).await;
        assert!(refused.is_err(), "an active run must block deletion");
        store
            .finish_run(
                &claims[0].run_id,
                RunOutcome::Succeeded { exit_code: Some(0) },
            )
            .await
            .unwrap();
        assert!(store.remove_definition(&created.id).await.unwrap());
        assert!(store.definition(&created.id).await.unwrap().is_none());
        assert!(store.runs(Some(&created.id), 10).await.unwrap().is_empty());
    }

    fn definition_with_misfire(
        trigger: ScheduleTrigger,
        misfire: MisfirePolicy,
        grace: Duration,
    ) -> NewScheduleDefinition {
        NewScheduleDefinition {
            misfire,
            misfire_grace: grace,
            ..definition(trigger)
        }
    }

    #[tokio::test]
    async fn a_missed_slot_is_recorded_once_and_never_executed() {
        let store = store().await;
        let now = leveler_core::now();
        let created = store
            .create_definition(definition_with_misfire(
                ScheduleTrigger::Every {
                    anchor: now - chrono::Duration::seconds(3600),
                    every: Duration::from_secs(60),
                },
                MisfirePolicy::Skip,
                Duration::from_secs(5),
            ))
            .await
            .unwrap();

        assert_eq!(store.reconcile_misfires(now).await.unwrap(), 1);
        assert!(
            store
                .claim_due(now, Duration::from_secs(60), 10)
                .await
                .unwrap()
                .is_empty(),
            "a skipped slot was claimed"
        );
        assert!(store.runs(Some(&created.id), 10).await.unwrap().is_empty());

        let occurrences = store.occurrences(&created.id, 10).await.unwrap();
        assert_eq!(occurrences.len(), 2);
        assert_eq!(
            occurrences
                .iter()
                .filter(|occurrence| occurrence.state == OccurrenceState::Missed)
                .count(),
            1
        );
        let pending = occurrences
            .iter()
            .find(|occurrence| occurrence.state == OccurrenceState::Pending)
            .expect("a recurring definition keeps a future slot");
        assert!(pending.scheduled_for > now);

        // The missed row is a terminal fact: policy never rewrites it.
        assert_eq!(store.reconcile_misfires(now).await.unwrap(), 0);
        assert_eq!(store.reconcile_misfires(now).await.unwrap(), 0);
    }

    #[tokio::test]
    async fn run_once_on_recovery_leaves_exactly_one_overdue_slot_for_this_pass() {
        let store = store().await;
        let now = leveler_core::now();
        let created = store
            .create_definition(definition_with_misfire(
                ScheduleTrigger::Every {
                    anchor: now - chrono::Duration::seconds(3600),
                    every: Duration::from_secs(60),
                },
                MisfirePolicy::RunOnceOnRecovery,
                Duration::from_secs(5),
            ))
            .await
            .unwrap();

        assert_eq!(store.reconcile_misfires(now).await.unwrap(), 0);
        let claims = store
            .claim_due(now, Duration::from_secs(60), 10)
            .await
            .unwrap();
        assert_eq!(claims.len(), 1, "the one overdue slot must be claimable");
        assert_eq!(store.reconcile_misfires(now).await.unwrap(), 0);

        let occurrences = store.occurrences(&created.id, 10).await.unwrap();
        assert_eq!(
            occurrences
                .iter()
                .filter(|occurrence| occurrence.state == OccurrenceState::Pending)
                .count(),
            0
        );
    }

    #[tokio::test]
    async fn a_slot_inside_the_grace_is_not_missed() {
        let store = store().await;
        let now = leveler_core::now();
        let created = store
            .create_definition(definition_with_misfire(
                ScheduleTrigger::Once {
                    at: now - chrono::Duration::seconds(10),
                },
                MisfirePolicy::Skip,
                Duration::from_secs(300),
            ))
            .await
            .unwrap();
        assert_eq!(store.reconcile_misfires(now).await.unwrap(), 0);
        assert_eq!(
            store
                .claim_due(now, Duration::from_secs(60), 10)
                .await
                .unwrap()
                .len(),
            1
        );
        let occurrences = store.occurrences(&created.id, 10).await.unwrap();
        assert_eq!(occurrences.len(), 1);
        assert_eq!(occurrences[0].state, OccurrenceState::Claimed);
    }

    /// One wake row per minute is cheap to write but unbounded over a year.
    /// History beyond the retention window is removed when a new pass starts;
    /// the current pass is never a candidate.
    #[tokio::test]
    async fn wake_history_is_pruned_beyond_the_retention_window() {
        let store = store().await;
        let old = store.begin_wake(WakeSource::Os).await.unwrap();
        sqlx::query("UPDATE schedule_wakeups SET started_at = ? WHERE id = ?")
            .bind(timestamp(
                leveler_core::now() - chrono::Duration::days(WAKE_RETENTION_DAYS + 1),
            ))
            .bind(old)
            .execute(&store.pool)
            .await
            .unwrap();
        let current = store.begin_wake(WakeSource::Manual).await.unwrap();
        let wakes = store.wakes(10).await.unwrap();
        assert_eq!(wakes.len(), 1, "the old wake was not pruned: {wakes:?}");
        assert_eq!(wakes[0].id, current);
    }

    #[tokio::test]
    async fn wake_records_show_started_unfinished_and_finished_passes() {
        let store = store().await;
        let os = store.begin_wake(WakeSource::Os).await.unwrap();
        let wakes = store.wakes(10).await.unwrap();
        assert_eq!(wakes.len(), 1);
        assert_eq!(wakes[0].id, os);
        assert_eq!(wakes[0].source, WakeSource::Os);
        assert!(wakes[0].finished_at.is_none(), "the pass is still running");
        assert_eq!(store.last_os_wake().await.unwrap().unwrap().id, os);

        let summary = DispatchSummary {
            recovered: 1,
            missed: 2,
            claimed: 3,
            succeeded: 4,
            submitted: 7,
            failed: 5,
            unknown: 6,
            ..DispatchSummary::default()
        };
        store.finish_wake(os, &summary).await.unwrap();
        let finished = &store.wakes(10).await.unwrap()[0];
        assert!(finished.finished_at.is_some());
        assert_eq!(
            (
                finished.recovered,
                finished.missed,
                finished.claimed,
                finished.succeeded,
                finished.failed,
                finished.unknown
            ),
            (1, 2, 3, 4, 5, 6)
        );
        assert_eq!(finished.submitted, 7, "submitted is not success");

        let manual = store.begin_wake(WakeSource::Manual).await.unwrap();
        store
            .fail_wake(manual, "database unavailable")
            .await
            .unwrap();
        let failed = &store.wakes(10).await.unwrap()[0];
        assert_eq!(failed.error.as_deref(), Some("database unavailable"));
        assert!(failed.finished_at.is_some());
        // The OS record remains the latest OS wake even after a newer manual
        // pass; the two facts are not conflated.
        assert_eq!(store.last_os_wake().await.unwrap().unwrap().id, os);
    }

    #[tokio::test]
    async fn lease_recovery_names_how_far_the_run_got() {
        let store = store().await;
        let now = leveler_core::now();
        let created = store
            .create_definition(definition(ScheduleTrigger::Every {
                anchor: now,
                every: Duration::from_secs(60),
            }))
            .await
            .unwrap();
        let claims = store
            .claim_due(now, Duration::from_secs(5), 10)
            .await
            .unwrap();
        assert_eq!(claims.len(), 1);
        let run_id = claims[0].run_id.clone();

        assert!(store.mark_delivered(&run_id).await.unwrap());
        let delivered = store.run(&run_id).await.unwrap().unwrap();
        assert!(delivered.delivered_at.is_some());
        assert!(delivered.running_at.is_none());
        assert_eq!(delivered.scheduled_for, claims[0].occurrence.scheduled_for);

        assert!(store.mark_running(&run_id, "ush-1").await.unwrap());
        let running = store.run(&run_id).await.unwrap().unwrap();
        assert_eq!(running.state, RunState::Running);
        assert!(running.running_at.is_some());

        let recovered = store
            .recover_expired(now + chrono::Duration::seconds(6))
            .await
            .unwrap();
        assert_eq!(recovered.len(), 1);
        let unknown = store.run(&run_id).await.unwrap().unwrap();
        assert_eq!(unknown.state, RunState::Unknown);
        assert!(
            unknown
                .error
                .as_deref()
                .unwrap()
                .contains("already started"),
            "{:?}",
            unknown.error
        );

        let occurrences = store.occurrences(&created.id, 10).await.unwrap();
        assert!(
            occurrences
                .iter()
                .any(|occurrence| occurrence.state == OccurrenceState::Interrupted)
        );
    }

    #[tokio::test]
    async fn lease_recovery_distinguishes_delivered_without_an_execution() {
        let store = store().await;
        let now = leveler_core::now();
        store
            .create_definition(definition(ScheduleTrigger::Every {
                anchor: now,
                every: Duration::from_secs(60),
            }))
            .await
            .unwrap();
        let claims = store
            .claim_due(now, Duration::from_secs(5), 10)
            .await
            .unwrap();
        assert!(store.mark_delivered(&claims[0].run_id).await.unwrap());
        store
            .recover_expired(now + chrono::Duration::seconds(6))
            .await
            .unwrap();
        let run = store.run(&claims[0].run_id).await.unwrap().unwrap();
        assert_eq!(run.state, RunState::Unknown);
        assert!(
            run.error
                .as_deref()
                .unwrap()
                .contains("acknowledged delivery"),
            "{:?}",
            run.error
        );
    }

    #[tokio::test]
    async fn lease_recovery_says_when_delivery_was_never_acknowledged() {
        let store = store().await;
        let now = leveler_core::now();
        store
            .create_definition(definition(ScheduleTrigger::Every {
                anchor: now,
                every: Duration::from_secs(60),
            }))
            .await
            .unwrap();
        let claims = store
            .claim_due(now, Duration::from_secs(5), 10)
            .await
            .unwrap();
        store
            .recover_expired(now + chrono::Duration::seconds(6))
            .await
            .unwrap();
        let run = store.run(&claims[0].run_id).await.unwrap().unwrap();
        assert_eq!(run.state, RunState::Unknown);
        assert!(
            run.error
                .as_deref()
                .unwrap()
                .contains("delivery was never acknowledged"),
            "{:?}",
            run.error
        );
    }
}
