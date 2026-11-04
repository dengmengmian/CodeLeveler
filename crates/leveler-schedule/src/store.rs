//! SQLite persistence for the global schedule domain.

use std::path::Path;
use std::time::Duration;

use chrono::{DateTime, Utc};
use leveler_client_protocol::{ModelRef, PermissionProfile, SessionId};
use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions};
use sqlx::{Pool, Row, Sqlite};

use crate::{
    DispatchSummary, MisfirePolicy, NewScheduleDefinition, Occurrence, OccurrenceId,
    OccurrenceState, RunId, RunState, ScheduleDefinition, ScheduleError, ScheduleId, ScheduleRun,
    ScheduleTrigger, WakeRecord, WakeSource, parse_timestamp, timestamp,
};

static MIGRATOR: sqlx::migrate::Migrator = sqlx::migrate!("./migrations");

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

/// The observed outcome of one run.
#[derive(Debug, Clone)]
pub enum RunOutcome {
    Succeeded {
        exit_code: Option<i32>,
    },
    Failed {
        exit_code: Option<i32>,
        error: String,
    },
    /// The effect cannot be proven. The occurrence is never retried.
    Unknown {
        error: String,
    },
}

impl RunOutcome {
    fn state(&self) -> RunState {
        match self {
            Self::Succeeded { .. } => RunState::Succeeded,
            Self::Failed { .. } => RunState::Failed,
            Self::Unknown { .. } => RunState::Unknown,
        }
    }

    fn exit_code(&self) -> Option<i32> {
        match self {
            Self::Succeeded { exit_code } | Self::Failed { exit_code, .. } => *exit_code,
            Self::Unknown { .. } => None,
        }
    }

    fn error(&self) -> Option<&str> {
        match self {
            Self::Succeeded { .. } => None,
            Self::Failed { error, .. } | Self::Unknown { error } => Some(error),
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
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect_with(options)
            .await?;
        MIGRATOR.run(&pool).await?;
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
        // The workspace names the runtime source that owns this schedule. The
        // runtime compares it by its canonical identity, so the durable value
        // must be canonical too; otherwise a macOS `/var` spelling would bind
        // the schedule to a source that refuses it.
        let workspace = tokio::fs::canonicalize(&new.workspace).await.map_err(|error| {
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
        let (trigger_kind, once_at, anchor_at, every_secs, first) = match new.trigger {
            ScheduleTrigger::Once { at } => ("once", Some(timestamp(at)), None, None, at),
            ScheduleTrigger::Every { anchor, every } => (
                "every",
                None,
                Some(timestamp(anchor)),
                Some(every.as_secs() as i64),
                anchor,
            ),
        };
        let mut tx = self.pool.begin().await?;
        sqlx::query(
            "INSERT INTO schedule_definitions \
             (id, name, workspace, action, command, trigger_kind, once_at, anchor_at, every_secs, \
              permission_mode, model, timeout_secs, misfire_policy, misfire_grace_secs, \
              session_id, enabled, created_at, updated_at) \
             VALUES (?, ?, ?, 'shell', ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, NULL, 1, ?, ?)",
        )
        .bind(id.as_str())
        .bind(new.name.trim())
        .bind(workspace.display().to_string())
        .bind(&new.command)
        .bind(trigger_kind)
        .bind(once_at)
        .bind(anchor_at)
        .bind(every_secs)
        .bind(new.mode.as_str())
        .bind(new.model.as_ref().map(|model| model.to_string()))
        .bind(new.timeout.as_secs() as i64)
        .bind(new.misfire.as_str())
        .bind(new.misfire_grace.as_secs() as i64)
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
        let rows = sqlx::query(
            "SELECT id, name, workspace, action, command, trigger_kind, once_at, anchor_at, \
             every_secs, permission_mode, model, timeout_secs, misfire_policy, \
             misfire_grace_secs, session_id, enabled, created_at \
             FROM schedule_definitions ORDER BY created_at, id",
        )
        .fetch_all(&self.pool)
        .await?;
        rows.iter().map(definition_from_row).collect()
    }

    /// One definition by id.
    pub async fn definition(
        &self,
        id: &ScheduleId,
    ) -> Result<Option<ScheduleDefinition>, ScheduleError> {
        let row = sqlx::query(
            "SELECT id, name, workspace, action, command, trigger_kind, once_at, anchor_at, \
             every_secs, permission_mode, model, timeout_secs, misfire_policy, \
             misfire_grace_secs, session_id, enabled, created_at \
             FROM schedule_definitions WHERE id = ?",
        )
        .bind(id.as_str())
        .fetch_optional(&self.pool)
        .await?;
        row.as_ref().map(definition_from_row).transpose()
    }

    /// Delete a definition, its occurrences and their runs.
    pub async fn remove_definition(&self, id: &ScheduleId) -> Result<bool, ScheduleError> {
        let result = sqlx::query("DELETE FROM schedule_definitions WHERE id = ?")
            .bind(id.as_str())
            .execute(&self.pool)
            .await?;
        Ok(result.rows_affected() == 1)
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

    /// Pending occurrences that are due, claimed atomically under `lease`.
    /// Candidates that lose the compare-and-set are skipped, not retried here.
    pub async fn claim_due(
        &self,
        now: DateTime<Utc>,
        lease: Duration,
        limit: usize,
    ) -> Result<Vec<Claim>, ScheduleError> {
        let candidates: Vec<(String, String)> = sqlx::query_as(
            "SELECT o.id, o.definition_id FROM schedule_occurrences o \
             JOIN schedule_definitions d ON d.id = o.definition_id \
             WHERE o.state = 'pending' AND o.scheduled_for <= ? AND d.enabled = 1 \
             ORDER BY o.scheduled_for, o.id LIMIT ?",
        )
        .bind(timestamp(now))
        .bind(limit as i64)
        .fetch_all(&self.pool)
        .await?;

        let mut claims = Vec::new();
        for (occurrence_id, definition_id) in candidates {
            let mut tx = self.pool.begin().await?;
            let updated = sqlx::query(
                "UPDATE schedule_occurrences SET state = 'claimed', claimed_at = ?, \
                 lease_expires_at = ? WHERE id = ? AND state = 'pending'",
            )
            .bind(timestamp(now))
            .bind(timestamp(lease_until(now, lease)))
            .bind(&occurrence_id)
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

            let Some(occurrence) = self.occurrence(&OccurrenceId::from(occurrence_id)).await? else {
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
             WHERE o.state = 'pending' AND o.scheduled_for <= ? AND d.enabled = 1",
        )
        .bind(timestamp(now))
        .fetch_all(&self.pool)
        .await?;

        let mut missed = 0;
        for (occurrence_id, definition_id, scheduled_for, policy, grace_secs) in candidates {
            let policy = MisfirePolicy::parse(&policy).ok_or_else(|| {
                ScheduleError::Invalid(format!("unsupported misfire policy `{policy}`"))
            })?;
            if policy != MisfirePolicy::Skip {
                continue;
            }
            let scheduled_for = parse_timestamp(&scheduled_for)?;
            let grace = chrono::Duration::seconds(grace_secs.max(0));
            if now - scheduled_for <= grace {
                continue;
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
            if let Some(definition) = self
                .definition(&ScheduleId::from(definition_id))
                .await?
            {
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
        let result = sqlx::query(
            "INSERT INTO schedule_wakeups (source, started_at) VALUES (?, ?)",
        )
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
            "UPDATE schedule_wakeups SET finished_at = ?, recovered = ?, missed = ?, \
             claimed = ?, succeeded = ?, failed = ?, unknown = ?, error = NULL WHERE id = ?",
        )
        .bind(timestamp(leveler_core::now()))
        .bind(summary.recovered as i64)
        .bind(summary.missed as i64)
        .bind(summary.claimed as i64)
        .bind(summary.succeeded as i64)
        .bind(summary.failed as i64)
        .bind(summary.unknown as i64)
        .bind(id)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Finish a wake record with a failure. The counts stay at their defaults
    /// and the error names what prevented a verdict.
    pub async fn fail_wake(&self, id: i64, error: &str) -> Result<(), ScheduleError> {
        sqlx::query(
            "UPDATE schedule_wakeups SET finished_at = ?, error = ? WHERE id = ?",
        )
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
            "SELECT id, source, started_at, finished_at, recovered, missed, claimed, \
             succeeded, failed, unknown, error FROM schedule_wakeups \
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
            "SELECT id, source, started_at, finished_at, recovered, missed, claimed, \
             succeeded, failed, unknown, error FROM schedule_wakeups \
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
             WHERE o.state = 'pending' AND o.scheduled_for <= ? AND d.enabled = 1",
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

    /// Settle the run and its occurrence. Idempotent: a run already settled
    /// (for example by lease recovery) is returned unchanged. `None` means the
    /// definition was removed while the run was in flight.
    pub async fn finish_run(
        &self,
        run_id: &RunId,
        outcome: RunOutcome,
    ) -> Result<Option<ScheduleRun>, ScheduleError> {
        let now = leveler_core::now();
        let mut tx = self.pool.begin().await?;
        let updated = sqlx::query(
            "UPDATE schedule_runs SET state = ?, exit_code = ?, error = ?, finished_at = ? \
             WHERE id = ? AND state IN ('dispatching', 'running')",
        )
        .bind(outcome.state().as_str())
        .bind(outcome.exit_code())
        .bind(outcome.error())
        .bind(timestamp(now))
        .bind(run_id.as_str())
        .execute(&mut *tx)
        .await?;
        if updated.rows_affected() == 1 {
            sqlx::query(
                "UPDATE schedule_occurrences SET state = 'completed' \
                 WHERE state = 'claimed' AND id = \
                 (SELECT occurrence_id FROM schedule_runs WHERE id = ?)",
            )
            .bind(run_id.as_str())
            .execute(&mut *tx)
            .await?;
        }
        tx.commit().await?;

        let run = self.run(run_id).await?;
        if let Some(run) = &run {
            if let Some(definition) = self.definition(&run.definition_id).await? {
                self.ensure_next_occurrence(&definition, now).await?;
            }
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
                sqlx::query(
                    "SELECT r.id, r.occurrence_id, r.definition_id, o.scheduled_for, r.state, \
                     r.session_id, r.execution_id, r.exit_code, r.error, r.started_at, \
                     r.delivered_at, r.running_at, r.finished_at FROM schedule_runs r \
                     JOIN schedule_occurrences o ON o.id = r.occurrence_id \
                     WHERE r.definition_id = ? ORDER BY r.started_at DESC, r.id DESC LIMIT ?",
                )
                .bind(id.as_str())
                .bind(limit as i64)
                .fetch_all(&self.pool)
                .await?
            }
            None => {
                sqlx::query(
                    "SELECT r.id, r.occurrence_id, r.definition_id, o.scheduled_for, r.state, \
                     r.session_id, r.execution_id, r.exit_code, r.error, r.started_at, \
                     r.delivered_at, r.running_at, r.finished_at FROM schedule_runs r \
                     JOIN schedule_occurrences o ON o.id = r.occurrence_id \
                     ORDER BY r.started_at DESC, r.id DESC LIMIT ?",
                )
                .bind(limit as i64)
                .fetch_all(&self.pool)
                .await?
            }
        };
        rows.iter().map(run_from_row).collect()
    }

    /// The earliest pending occurrence across enabled definitions.
    pub async fn next_due(&self) -> Result<Option<DateTime<Utc>>, ScheduleError> {
        let raw: Option<String> = sqlx::query_scalar(
            "SELECT MIN(o.scheduled_for) FROM schedule_occurrences o \
             JOIN schedule_definitions d ON d.id = o.definition_id \
             WHERE o.state = 'pending' AND d.enabled = 1",
        )
        .fetch_one(&self.pool)
        .await?;
        raw.as_deref().map(parse_timestamp).transpose()
    }

    /// Occurrences, newest first, for display and verification.
    pub async fn occurrences(
        &self,
        definition_id: &ScheduleId,
        limit: usize,
    ) -> Result<Vec<Occurrence>, ScheduleError> {
        let rows = sqlx::query(
            "SELECT id, definition_id, scheduled_for, state FROM schedule_occurrences \
             WHERE definition_id = ? ORDER BY scheduled_for DESC LIMIT ?",
        )
        .bind(definition_id.as_str())
        .bind(limit as i64)
        .fetch_all(&self.pool)
        .await?;
        rows.iter().map(occurrence_from_row).collect()
    }

    async fn occurrence(
        &self,
        id: &OccurrenceId,
    ) -> Result<Option<Occurrence>, ScheduleError> {
        let row = sqlx::query(
            "SELECT id, definition_id, scheduled_for, state FROM schedule_occurrences WHERE id = ?",
        )
        .bind(id.as_str())
        .fetch_optional(&self.pool)
        .await?;
        row.as_ref().map(occurrence_from_row).transpose()
    }

    /// One run by id.
    pub async fn run(&self, id: &RunId) -> Result<Option<ScheduleRun>, ScheduleError> {
        let row = sqlx::query(
            "SELECT r.id, r.occurrence_id, r.definition_id, o.scheduled_for, r.state, \
             r.session_id, r.execution_id, r.exit_code, r.error, r.started_at, \
             r.delivered_at, r.running_at, r.finished_at FROM schedule_runs r \
             JOIN schedule_occurrences o ON o.id = r.occurrence_id WHERE r.id = ?",
        )
        .bind(id.as_str())
        .fetch_optional(&self.pool)
        .await?;
        row.as_ref().map(run_from_row).transpose()
    }

    /// Create the next future occurrence for an interval definition. Missed
    /// slots are skipped, never generated in a burst, and the unique
    /// `(definition, scheduled_for)` key makes this safe to call twice.
    pub async fn ensure_next_occurrence(
        &self,
        definition: &ScheduleDefinition,
        now: DateTime<Utc>,
    ) -> Result<Option<Occurrence>, ScheduleError> {
        if !definition.enabled {
            return Ok(None);
        }
        let ScheduleTrigger::Every { anchor, every } = definition.trigger else {
            return Ok(None);
        };
        let every_secs = every.as_secs();
        if every_secs == 0 {
            return Err(ScheduleError::Invalid(
                "an interval schedule must be at least one second".into(),
            ));
        }
        let every = chrono::Duration::seconds(every_secs as i64);
        let last: Option<String> = sqlx::query_scalar(
            "SELECT MAX(scheduled_for) FROM schedule_occurrences WHERE definition_id = ?",
        )
        .bind(definition.id.as_str())
        .fetch_one(&self.pool)
        .await?;
        let last = last.as_deref().map(parse_timestamp).transpose()?;
        let next = match last {
            Some(last) if last + every > now => last + every,
            _ if now < anchor => anchor,
            _ => {
                // The first slot strictly after now, on the anchor's grid.
                // Computed from now instead of a slot index so a very old
                // anchor cannot overflow the interval multiplication.
                let remainder = (now - anchor).num_seconds().rem_euclid(every_secs as i64);
                now + chrono::Duration::seconds(every_secs as i64 - remainder)
            }
        };
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
            "SELECT id, definition_id, scheduled_for, state FROM schedule_occurrences \
             WHERE definition_id = ? AND scheduled_for = ?",
        )
        .bind(definition.id.as_str())
        .bind(timestamp(next))
        .fetch_optional(&self.pool)
        .await?;
        row.as_ref().map(occurrence_from_row).transpose()
    }
}

fn definition_from_row(row: &sqlx::sqlite::SqliteRow) -> Result<ScheduleDefinition, ScheduleError> {
    let action: String = row.try_get("action")?;
    if action != "shell" {
        return Err(ScheduleError::Invalid(format!(
            "unsupported schedule action `{action}`"
        )));
    }
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
        other => {
            return Err(ScheduleError::Invalid(format!(
                "unsupported schedule trigger `{other}`"
            )));
        }
    };
    let permission_mode: String = row.try_get("permission_mode")?;
    let mode = PermissionProfile::parse(&permission_mode).ok_or_else(|| {
        ScheduleError::Invalid(format!("unsupported permission mode `{permission_mode}`"))
    })?;
    let model: Option<String> = row.try_get("model")?;
    let model = match model {
        Some(raw) => Some(ModelRef::parse(&raw).ok_or_else(|| {
            ScheduleError::Invalid(format!("invalid model reference `{raw}`"))
        })?),
        None => None,
    };
    let timeout_secs: i64 = row.try_get("timeout_secs")?;
    let misfire_policy: String = row.try_get("misfire_policy")?;
    let misfire = MisfirePolicy::parse(&misfire_policy).ok_or_else(|| {
        ScheduleError::Invalid(format!(
            "unsupported misfire policy `{misfire_policy}`"
        ))
    })?;
    let misfire_grace_secs: i64 = row.try_get("misfire_grace_secs")?;
    let session: Option<String> = row.try_get("session_id")?;
    let created_at: String = row.try_get("created_at")?;
    Ok(ScheduleDefinition {
        id: ScheduleId::from(row.try_get::<String, _>("id")?),
        name: row.try_get("name")?,
        workspace: std::path::PathBuf::from(row.try_get::<String, _>("workspace")?),
        command: row.try_get("command")?,
        trigger,
        mode,
        model,
        timeout: Duration::from_secs(timeout_secs.max(1) as u64),
        misfire,
        misfire_grace: Duration::from_secs(misfire_grace_secs.max(0) as u64),
        session_id: session.map(SessionId::from),
        enabled: row.try_get::<i64, _>("enabled")? != 0,
        created_at: parse_timestamp(&created_at)?,
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
        state: RunState::parse(&state).ok_or_else(|| {
            ScheduleError::Invalid(format!("unsupported run state `{state}`"))
        })?,
        session_id: session.map(SessionId::from),
        execution_id: row.try_get("execution_id")?,
        exit_code: row.try_get("exit_code")?,
        error: row.try_get("error")?,
        started_at: parse_timestamp(&row.try_get::<String, _>("started_at")?)?,
        delivered_at: optional_timestamp(row, "delivered_at")?,
        running_at: optional_timestamp(row, "running_at")?,
        finished_at: optional_timestamp(row, "finished_at")?,
    })
}

fn wake_from_row(row: &sqlx::sqlite::SqliteRow) -> Result<WakeRecord, ScheduleError> {
    let source: String = row.try_get("source")?;
    Ok(WakeRecord {
        id: row.try_get("id")?,
        source: WakeSource::parse(&source).ok_or_else(|| {
            ScheduleError::Invalid(format!("unsupported wake source `{source}`"))
        })?,
        started_at: parse_timestamp(&row.try_get::<String, _>("started_at")?)?,
        finished_at: optional_timestamp(row, "finished_at")?,
        recovered: usize_of(row, "recovered")?,
        missed: usize_of(row, "missed")?,
        claimed: usize_of(row, "claimed")?,
        succeeded: usize_of(row, "succeeded")?,
        failed: usize_of(row, "failed")?,
        unknown: usize_of(row, "unknown")?,
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

    fn definition(trigger: ScheduleTrigger) -> NewScheduleDefinition {
        let workspace = std::env::temp_dir().join("leveler-schedule-test");
        std::fs::create_dir_all(&workspace).unwrap();
        NewScheduleDefinition {
            name: "nightly".into(),
            workspace,
            command: "printf ok".into(),
            trigger,
            mode: PermissionProfile::Assisted,
            model: None,
            timeout: Duration::from_secs(60),
            misfire: MisfirePolicy::Skip,
            misfire_grace: Duration::from_secs(300),
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
                RunOutcome::Succeeded {
                    exit_code: Some(0),
                },
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
                RunOutcome::Succeeded {
                    exit_code: Some(0),
                },
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
            failed: 5,
            unknown: 6,
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

        let manual = store.begin_wake(WakeSource::Manual).await.unwrap();
        store.fail_wake(manual, "database unavailable").await.unwrap();
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
