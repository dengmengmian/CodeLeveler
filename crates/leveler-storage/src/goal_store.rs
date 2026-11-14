//! The `GoalStore` port: durable goal identity.
//!
//! A goal is a long-lived intent; a turn is one execution of it. Until this
//! port existed only the turn had a durable record, so a process that died
//! mid-goal left nothing saying work was owed — the gap the long-goal audit
//! named as the one thing "long-running" means and CodeLeveler did not do.
//!
//! Same shape as [`crate::TaskStore`]: a narrow trait the engine depends on,
//! storage owns the SQLite adapter, and [`MemoryGoalStore`] exercises the
//! identical contract without SQLite.
//!
//! **This port records identity, not outcome.** Whether a goal succeeded lives
//! on the session row, which is still the single lifecycle writer. `state`
//! here answers only: does this goal still owe work?

use std::sync::Mutex;

use async_trait::async_trait;

use leveler_core::{GoalId, OwnershipToken, TaskId, Timestamp, TurnId};
use leveler_lifecycle::{
    AppliedRequirementEvent, IntentProjection, PauseBoundary, RequirementItem, RequirementKind,
    RequirementSeed, RequirementVerification, ScopeDeclaration, ScopeError, ScopeOperationKind,
    ScopeProjection, UserIntentDeclaration, UserIntentOperation, apply_scope_declaration,
    fold_scope_events, initial_scope,
};

use crate::{Database, OwnershipError, StorageError};

/// Does this goal still owe work?
///
/// Two states on purpose. There is deliberately no `Interrupted`: a goal left
/// `Running` by a process that died IS the interrupted one, and task ownership
/// already records whether a live runtime is driving it. A third state would
/// be a second writer for a fact ownership already holds.
///
/// `Running` is also the state of a goal whose window ended without delivering
/// it — blocked, recoverably failed, interrupted, or stopped at a budget. The
/// work is still owed and an explicit continuation re-enters it, which is why
/// those windows do not settle the goal. `Settled` means no window can follow
/// this goal any more: it was delivered, the user cancelled it, or the runtime
/// classified the failure as unrecoverable. How it went is `sessions.outcome`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GoalState {
    /// Work is owed and a continuation can re-enter it. Whether anyone is
    /// currently driving it is an ownership question, not a goal-state
    /// question.
    Running,
    /// Nothing can follow this goal up any more. How it went is
    /// `sessions.outcome`.
    Settled,
}

impl GoalState {
    /// The persisted spelling.
    pub fn as_str(self) -> &'static str {
        match self {
            GoalState::Running => "running",
            GoalState::Settled => "settled",
        }
    }

    /// Parse a persisted state. Unknown values are refused rather than
    /// defaulted: a row we cannot interpret must not silently become
    /// "settled" and stop being reported as owed work.
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "running" => Some(GoalState::Running),
            "settled" => Some(GoalState::Settled),
            _ => None,
        }
    }
}

/// One durable goal.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GoalRecord {
    /// Stable identity, independent of any process or turn.
    pub id: GoalId,
    /// The task this goal was opened against. One task hosts many goals.
    pub task_id: TaskId,
    /// What the user asked, verbatim.
    pub objective: String,
    /// Whether work is still owed. Never the success/failure verdict.
    pub state: GoalState,
    /// When the goal was opened.
    pub opened_at: Timestamp,
    /// When it stopped owing work; `None` while running.
    pub settled_at: Option<Timestamp>,
    /// Work windows consumed. Durable because the count has to survive the
    /// process that ran them.
    pub windows_run: u32,
    /// The model's candidate acceptance checks, declared once at commit. This
    /// is a plan, not the user's scope; the runtime never lets a later turn
    /// replace it.
    pub acceptance: Vec<String>,
    /// Verbatim user turns that adjust the task scope after the objective.
    /// A waiver here is the only way a requirement leaves the effective set.
    pub amendments: Vec<String>,
    /// When the user's own stage pause was reached, if the goal is waiting for
    /// their continuation authorization. `None` = not paused.
    ///
    /// This is an authorization fact, not a verdict: `state` stays `running`
    /// while a paused goal still owes the work the user withheld permission to
    /// start. It is set only from a validated
    /// [`leveler_lifecycle::UserIntentDeclaration`] that the model quoted from
    /// the user's own message, and cleared only by a later declaration that
    /// authorizes continuing (or by settling).
    pub user_paused_at: Option<Timestamp>,
    /// The structured boundary the user's pause waits on. `None` = no pending
    /// boundary. Replaced or released by a later declaration; never derived
    /// from natural-language text by a client or a reader.
    pub pause_boundary: Option<PauseBoundary>,
}

/// One applied user-intent declaration, in order, for audit and replay.
///
/// This is history, not authority: the current authorization state lives on
/// [`GoalRecord`]. The log makes "who authorized this, with which words, in
/// which order" answerable after a restart. `turn_id` identifies the window
/// whose transcript holds the raw user message the quote was taken from; the
/// transcript row, not this log, remains the message's authoritative copy.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GoalIntentRecord {
    /// Per-goal monotonic sequence, starting at 1.
    pub seq: u32,
    /// The turn whose newest user input the quote came from.
    pub turn_id: String,
    /// The span the model quoted from the user's newest input.
    pub source_quote: String,
    /// The ordered operations the runtime accepted.
    pub operations: Vec<UserIntentOperation>,
    /// Authorization state after applying the declaration.
    pub pause_in_force: bool,
    /// Boundary after applying the declaration.
    pub pause_boundary: Option<PauseBoundary>,
    /// When the declaration was accepted.
    pub created_at: Timestamp,
}

/// One accepted requirement event, in order, for audit and replay.
///
/// History, not authority: the current requirement set is folded from these
/// rows. `turn_id` identifies the window whose transcript holds the user span
/// the event quotes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GoalRequirementRecord {
    /// Per-goal monotonic sequence, starting at 1.
    pub seq: u32,
    /// The turn whose newest user input the quote came from.
    pub turn_id: String,
    /// The accepted operation and its normalized payload.
    pub event: AppliedRequirementEvent,
    /// When the event was accepted.
    pub created_at: Timestamp,
}

/// Why a scope write did not apply.
///
/// The declaration is checked against the goal's own history: an unknown
/// requirement identity, an illegal transition and a malformed event chain are
/// distinguished from ownership or storage failure so the model can correct the
/// declaration instead of retrying blindly.
#[derive(Debug, thiserror::Error)]
pub enum ScopeWriteError {
    /// The caller does not own the task, the goal is settled, or the store
    /// failed.
    #[error(transparent)]
    Ownership(#[from] OwnershipError),
    /// The declaration itself was refused.
    #[error(transparent)]
    Scope(#[from] ScopeError),
}

impl From<StorageError> for ScopeWriteError {
    fn from(error: StorageError) -> Self {
        Self::Ownership(OwnershipError::Storage(error))
    }
}

/// Access to durable goals.
#[async_trait]
pub trait GoalStore: Send + Sync {
    /// Open a goal against the task currently owned by `token`, recording the
    /// model's candidate acceptance checks and its initial requirement
    /// declarations in the same atomic write.
    ///
    /// The ownership assertion, the goal insert and its requirement events are
    /// one transaction. A stale token creates no goal and returns a typed
    /// ownership error; an invalid seed creates neither.
    async fn open_scoped(
        &self,
        token: &OwnershipToken,
        objective: &str,
        acceptance: &[String],
        requirements: &[RequirementSeed],
        now: Timestamp,
    ) -> Result<GoalId, OwnershipError>;

    /// Open a goal with no model-declared acceptance (a `/goal` turn, or a
    /// caller that has no candidate plan yet).
    async fn open(
        &self,
        token: &OwnershipToken,
        objective: &str,
        now: Timestamp,
    ) -> Result<GoalId, OwnershipError> {
        self.open_scoped(token, objective, &[], &[], now).await
    }

    /// Append one verbatim user amendment to the goal's scope. Idempotent for
    /// text already recorded. Only a running goal may gain one.
    async fn append_amendment(
        &self,
        token: &OwnershipToken,
        goal_id: &GoalId,
        amendment: &str,
    ) -> Result<(), OwnershipError>;

    /// Note that a work window ran. Idempotency is the caller's business —
    /// this counts calls, because a window that ran twice really is two
    /// windows.
    async fn note_window(
        &self,
        token: &OwnershipToken,
        goal_id: &GoalId,
    ) -> Result<(), OwnershipError>;

    /// Mark a goal as owing no further work. Settling twice is a no-op rather
    /// than an error: the terminal path can be reached from more than one
    /// place, and the second caller is not wrong.
    async fn settle(
        &self,
        token: &OwnershipToken,
        goal_id: &GoalId,
        now: Timestamp,
    ) -> Result<(), OwnershipError>;

    /// Record (or clear) the user's stage pause on this goal.
    ///
    /// `Some(now)` states that the user's own boundary was reached and the goal
    /// is waiting for their authorization; `None` clears it because a later
    /// user instruction authorized continuing (or the goal settled). Idempotent:
    /// the same write twice is not an error.
    async fn set_user_pause(
        &self,
        token: &OwnershipToken,
        goal_id: &GoalId,
        paused_at: Option<Timestamp>,
    ) -> Result<(), OwnershipError>;

    /// Apply one validated user-intent declaration atomically: the durable
    /// pause/boundary state, an optional cancellation, and the ordered history
    /// row are one write.
    ///
    /// The caller has already validated that `declaration.source_quote` occurs
    /// in the newest user input and that `projection` is a legal transition.
    /// This method owns only identity, ownership, settled-state refusal and
    /// atomicity.
    async fn record_intent(
        &self,
        token: &OwnershipToken,
        goal_id: &GoalId,
        turn_id: &TurnId,
        declaration: &UserIntentDeclaration,
        projection: &IntentProjection,
        now: Timestamp,
    ) -> Result<(), OwnershipError>;

    /// The ordered intent history for one goal, oldest first.
    async fn intent_log(&self, goal_id: &GoalId) -> Result<Vec<GoalIntentRecord>, StorageError>;

    /// Apply one scope declaration atomically: fold the goal's recorded
    /// history, validate the declaration against it, and append the accepted
    /// events in order.
    ///
    /// The caller has already validated that each operation's `source_quote`
    /// occurs in the user input that authorized it. This method owns identity,
    /// ownership, settled-state refusal, transition legality and atomicity.
    async fn record_scope(
        &self,
        token: &OwnershipToken,
        goal_id: &GoalId,
        turn_id: &TurnId,
        declaration: &ScopeDeclaration,
        now: Timestamp,
    ) -> Result<ScopeProjection, ScopeWriteError>;

    /// The ordered requirement-event history for one goal, oldest first.
    async fn requirement_log(
        &self,
        goal_id: &GoalId,
    ) -> Result<Vec<GoalRequirementRecord>, StorageError>;

    /// The current requirement set, folded from the recorded history.
    async fn requirements(&self, goal_id: &GoalId) -> Result<Vec<RequirementItem>, StorageError> {
        let records = self.requirement_log(goal_id).await?;
        let events: Vec<AppliedRequirementEvent> =
            records.into_iter().map(|record| record.event).collect();
        fold_scope_events(&events).map_err(|error| {
            StorageError::InvalidData(format!(
                "goal {goal_id} requirement history cannot be folded: {error}"
            ))
        })
    }

    /// Reopen this exact settled goal for an explicit continuation. This does
    /// not create or select a goal by objective text; ownership and task
    /// identity are checked atomically with the state transition.
    async fn reopen(&self, token: &OwnershipToken, goal_id: &GoalId) -> Result<(), OwnershipError>;

    /// Capabilities exposed for this exact goal, in deterministic id order.
    async fn active_capabilities(&self, goal_id: &GoalId) -> Result<Vec<String>, StorageError>;

    /// Append one capability under current ownership. Idempotent; no unload.
    /// A settled goal must be reopened before it can load additional tools.
    async fn enable_capability(
        &self,
        token: &OwnershipToken,
        goal_id: &GoalId,
        capability_id: &str,
    ) -> Result<(), OwnershipError>;

    /// One goal by id, or `None` when it does not exist.
    async fn get(&self, goal_id: &GoalId) -> Result<Option<GoalRecord>, StorageError>;

    /// Every goal still owing work, newest first.
    ///
    /// This is the question a restart asks. It deliberately does not filter by
    /// owner: reporting a goal owned by another runtime is better than
    /// silently omitting work that exists.
    async fn unfinished(&self) -> Result<Vec<GoalRecord>, StorageError>;

    /// Goals belonging to one task, newest first. One task hosts many goals.
    async fn for_task(&self, task_id: &TaskId) -> Result<Vec<GoalRecord>, StorageError>;
}

/// The production SQLite adapter over the `goals` table (migration 0018).
#[async_trait]
impl GoalStore for Database {
    async fn open_scoped(
        &self,
        token: &OwnershipToken,
        objective: &str,
        acceptance: &[String],
        requirements: &[RequirementSeed],
        now: Timestamp,
    ) -> Result<GoalId, OwnershipError> {
        let initial = initial_scope(requirements).map_err(|error| {
            OwnershipError::Storage(StorageError::InvalidData(format!(
                "invalid initial requirements: {error}"
            )))
        })?;
        let id = GoalId::new(leveler_core::new_uuid_string());
        let acceptance = serde_json::to_string(acceptance)
            .map_err(|e| StorageError::InvalidData(format!("unencodable acceptance: {e}")))?;
        let mut tx = self
            .pool()
            .begin_with("BEGIN IMMEDIATE")
            .await
            .map_err(StorageError::from)?;
        let inserted = sqlx::query(
            "INSERT INTO goals \
             (id, task_id, objective, state, opened_at, windows_run, acceptance, amendments) \
             SELECT ?1, id, ?5, ?6, ?7, 0, ?8, '[]' FROM tasks \
             WHERE id = ?2 AND owner_runtime_id = ?3 AND owner_epoch = ?4",
        )
        .bind(id.as_str())
        .bind(token.task_id.as_str())
        .bind(token.runtime_id.as_str())
        .bind(token.owner_epoch.get() as i64)
        .bind(objective)
        .bind(GoalState::Running.as_str())
        .bind(now.to_rfc3339())
        .bind(acceptance)
        .execute(&mut *tx)
        .await
        .map_err(StorageError::from)?;
        if inserted.rows_affected() != 1 {
            tx.rollback().await.map_err(StorageError::from)?;
            return Err(crate::ownership_store::sqlite_stale_error(self, token).await);
        }
        for (offset, event) in initial.events.iter().enumerate() {
            insert_requirement_event(&mut tx, id.as_str(), offset as i64 + 1, "", event, now)
                .await?;
        }
        tx.commit().await.map_err(StorageError::from)?;
        Ok(id)
    }

    async fn append_amendment(
        &self,
        token: &OwnershipToken,
        goal_id: &GoalId,
        amendment: &str,
    ) -> Result<(), OwnershipError> {
        let amendment = amendment.trim();
        if amendment.is_empty() {
            return Ok(());
        }
        // BEGIN IMMEDIATE, not a deferred BEGIN: this transaction reads and
        // then writes, and the runtime's other lanes (command receipts, the
        // event log) write to the same database while a user message is being
        // taken in. A deferred BEGIN takes a read lock first, and the UPDATE
        // that follows then has to upgrade it; SQLite answers a losing upgrade
        // with SQLITE_BUSY immediately and does not retry (deadlock avoidance),
        // which aborted the whole turn over a user's own message. Taking the
        // write lock at BEGIN makes the wait happen where SQLite does retry.
        let mut tx = self
            .pool()
            .begin_with("BEGIN IMMEDIATE")
            .await
            .map_err(StorageError::from)?;
        let row: Option<(String, String)> = sqlx::query_as(
            "SELECT goals.amendments, goals.state FROM goals \
             JOIN tasks ON tasks.id = goals.task_id \
             WHERE goals.id = ?1 AND goals.task_id = ?2 \
             AND tasks.owner_runtime_id = ?3 AND tasks.owner_epoch = ?4",
        )
        .bind(goal_id.as_str())
        .bind(token.task_id.as_str())
        .bind(token.runtime_id.as_str())
        .bind(token.owner_epoch.get() as i64)
        .fetch_optional(&mut *tx)
        .await
        .map_err(StorageError::from)?;
        let Some((amendments, state)) = row else {
            tx.rollback().await.map_err(StorageError::from)?;
            return goal_write_miss(self, token, goal_id, false).await;
        };
        if state != GoalState::Running.as_str() {
            tx.rollback().await.map_err(StorageError::from)?;
            return Err(StorageError::InvalidData(format!(
                "goal {goal_id} is settled; a settled goal cannot gain an amendment"
            ))
            .into());
        }
        let mut values = parse_list(&amendments, "amendments")?;
        if !values.iter().any(|value| value == amendment) {
            values.push(amendment.to_string());
            let encoded = serde_json::to_string(&values)
                .map_err(|e| StorageError::InvalidData(format!("unencodable amendment: {e}")))?;
            sqlx::query("UPDATE goals SET amendments = ?3 WHERE id = ?1 AND task_id = ?2")
                .bind(goal_id.as_str())
                .bind(token.task_id.as_str())
                .bind(encoded)
                .execute(&mut *tx)
                .await
                .map_err(StorageError::from)?;
        }
        tx.commit().await.map_err(StorageError::from)?;
        Ok(())
    }

    async fn note_window(
        &self,
        token: &OwnershipToken,
        goal_id: &GoalId,
    ) -> Result<(), OwnershipError> {
        let updated = sqlx::query(
            "UPDATE goals SET windows_run = windows_run + 1 \
             WHERE id = ?1 AND task_id = ?2 AND EXISTS (\
                 SELECT 1 FROM tasks WHERE id = ?2 \
                 AND owner_runtime_id = ?3 AND owner_epoch = ?4\
             )",
        )
        .bind(goal_id.as_str())
        .bind(token.task_id.as_str())
        .bind(token.runtime_id.as_str())
        .bind(token.owner_epoch.get() as i64)
        .execute(self.pool())
        .await
        .map_err(StorageError::from)?;
        if updated.rows_affected() == 1 {
            return Ok(());
        }
        goal_write_miss(self, token, goal_id, false).await
    }

    async fn settle(
        &self,
        token: &OwnershipToken,
        goal_id: &GoalId,
        now: Timestamp,
    ) -> Result<(), OwnershipError> {
        // `state = 'running'` in the predicate makes a second settle a no-op
        // and keeps the first settled_at, which is the one that is true.
        let updated = sqlx::query(
            "UPDATE goals SET state = ?2, settled_at = ?3, user_paused_at = NULL, \
             pause_boundary = NULL \
             WHERE id = ?1 AND task_id = ?4 AND state = 'running' AND EXISTS (\
                 SELECT 1 FROM tasks WHERE id = ?4 \
                 AND owner_runtime_id = ?5 AND owner_epoch = ?6\
             )",
        )
        .bind(goal_id.as_str())
        .bind(GoalState::Settled.as_str())
        .bind(now.to_rfc3339())
        .bind(token.task_id.as_str())
        .bind(token.runtime_id.as_str())
        .bind(token.owner_epoch.get() as i64)
        .execute(self.pool())
        .await
        .map_err(StorageError::from)?;
        if updated.rows_affected() == 1 {
            return Ok(());
        }
        goal_write_miss(self, token, goal_id, true).await
    }

    async fn reopen(&self, token: &OwnershipToken, goal_id: &GoalId) -> Result<(), OwnershipError> {
        let updated = sqlx::query(
            "UPDATE goals SET state = 'running', settled_at = NULL, user_paused_at = NULL, \
             pause_boundary = NULL \
             WHERE id = ?1 AND task_id = ?2 AND state IN ('settled', 'running') AND EXISTS (\
                 SELECT 1 FROM tasks WHERE id = ?2 \
                 AND owner_runtime_id = ?3 AND owner_epoch = ?4\
             )",
        )
        .bind(goal_id.as_str())
        .bind(token.task_id.as_str())
        .bind(token.runtime_id.as_str())
        .bind(token.owner_epoch.get() as i64)
        .execute(self.pool())
        .await
        .map_err(StorageError::from)?;
        if updated.rows_affected() == 1 {
            return Ok(());
        }
        goal_write_miss(self, token, goal_id, false).await
    }

    async fn set_user_pause(
        &self,
        token: &OwnershipToken,
        goal_id: &GoalId,
        paused_at: Option<Timestamp>,
    ) -> Result<(), OwnershipError> {
        // `paused_at = COALESCE(?, paused_at)` would make clearing impossible;
        // the bind states the whole value, so a release really clears it.
        let updated = sqlx::query(
            "UPDATE goals SET user_paused_at = ?3 \
             WHERE id = ?1 AND task_id = ?2 AND EXISTS (\
                 SELECT 1 FROM tasks WHERE id = ?2 \
                 AND owner_runtime_id = ?4 AND owner_epoch = ?5\
             )",
        )
        .bind(goal_id.as_str())
        .bind(token.task_id.as_str())
        .bind(paused_at.map(|at| at.to_rfc3339()))
        .bind(token.runtime_id.as_str())
        .bind(token.owner_epoch.get() as i64)
        .execute(self.pool())
        .await
        .map_err(StorageError::from)?;
        if updated.rows_affected() == 1 {
            return Ok(());
        }
        goal_write_miss(self, token, goal_id, false).await
    }

    async fn record_intent(
        &self,
        token: &OwnershipToken,
        goal_id: &GoalId,
        turn_id: &TurnId,
        declaration: &UserIntentDeclaration,
        projection: &IntentProjection,
        now: Timestamp,
    ) -> Result<(), OwnershipError> {
        let mut tx = self
            .pool()
            .begin_with("BEGIN IMMEDIATE")
            .await
            .map_err(StorageError::from)?;
        let row: Option<(String,)> = sqlx::query_as(
            "SELECT goals.state FROM goals \
             JOIN tasks ON tasks.id = goals.task_id \
             WHERE goals.id = ?1 AND goals.task_id = ?2 \
             AND tasks.owner_runtime_id = ?3 AND tasks.owner_epoch = ?4",
        )
        .bind(goal_id.as_str())
        .bind(token.task_id.as_str())
        .bind(token.runtime_id.as_str())
        .bind(token.owner_epoch.get() as i64)
        .fetch_optional(&mut *tx)
        .await
        .map_err(StorageError::from)?;
        let Some((state,)) = row else {
            tx.rollback().await.map_err(StorageError::from)?;
            return goal_write_miss(self, token, goal_id, false).await;
        };
        if state != GoalState::Running.as_str() && !projection.cancel {
            tx.rollback().await.map_err(StorageError::from)?;
            return Err(StorageError::InvalidData(format!(
                "goal {goal_id} is settled; an intent that does not reopen it cannot apply"
            ))
            .into());
        }
        let boundary = projection
            .pause_boundary
            .as_ref()
            .map(serde_json::to_string)
            .transpose()
            .map_err(|error| {
                StorageError::InvalidData(format!("unencodable pause boundary: {error}"))
            })?;
        let operations = serde_json::to_string(&declaration.operations).map_err(|error| {
            StorageError::InvalidData(format!("unencodable intent operations: {error}"))
        })?;
        if projection.cancel {
            sqlx::query(
                "UPDATE goals SET state = ?3, settled_at = ?4, user_paused_at = NULL, \
                 pause_boundary = NULL WHERE id = ?1 AND task_id = ?2",
            )
            .bind(goal_id.as_str())
            .bind(token.task_id.as_str())
            .bind(GoalState::Settled.as_str())
            .bind(now.to_rfc3339())
            .execute(&mut *tx)
            .await
            .map_err(StorageError::from)?;
        } else {
            sqlx::query(
                "UPDATE goals SET user_paused_at = ?3, pause_boundary = ?4 \
                 WHERE id = ?1 AND task_id = ?2",
            )
            .bind(goal_id.as_str())
            .bind(token.task_id.as_str())
            .bind(projection.pause_in_force.then(|| now.to_rfc3339()))
            .bind(boundary)
            .execute(&mut *tx)
            .await
            .map_err(StorageError::from)?;
        }
        let seq: i64 = sqlx::query_scalar(
            "SELECT COALESCE(MAX(seq), 0) + 1 FROM goal_intents WHERE goal_id = ?1",
        )
        .bind(goal_id.as_str())
        .fetch_one(&mut *tx)
        .await
        .map_err(StorageError::from)?;
        sqlx::query(
            "INSERT INTO goal_intents \
             (goal_id, seq, turn_id, source_quote, operations, pause_in_force, pause_boundary, \
              created_at) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
        )
        .bind(goal_id.as_str())
        .bind(seq)
        .bind(turn_id.as_str())
        .bind(&declaration.source_quote)
        .bind(operations)
        .bind(i64::from(projection.pause_in_force))
        .bind(
            projection
                .pause_boundary
                .as_ref()
                .map(serde_json::to_string)
                .transpose()
                .map_err(|error| {
                    StorageError::InvalidData(format!("unencodable pause boundary: {error}"))
                })?,
        )
        .bind(now.to_rfc3339())
        .execute(&mut *tx)
        .await
        .map_err(StorageError::from)?;
        tx.commit().await.map_err(StorageError::from)?;
        Ok(())
    }

    async fn intent_log(&self, goal_id: &GoalId) -> Result<Vec<GoalIntentRecord>, StorageError> {
        let rows: Vec<(i64, String, String, String, i64, Option<String>, String)> = sqlx::query_as(
            "SELECT seq, turn_id, source_quote, operations, pause_in_force, pause_boundary, \
             created_at \
             FROM goal_intents WHERE goal_id = ?1 ORDER BY seq",
        )
        .bind(goal_id.as_str())
        .fetch_all(self.pool())
        .await?;
        rows.into_iter()
            .map(
                |(seq, turn_id, source_quote, operations, pause_in_force, boundary, created_at)| {
                    Ok(GoalIntentRecord {
                        seq: seq.max(0) as u32,
                        turn_id,
                        source_quote,
                        operations: serde_json::from_str(&operations).map_err(|error| {
                            StorageError::InvalidData(format!(
                                "bad goal intent operations JSON `{operations}`: {error}"
                            ))
                        })?,
                        pause_in_force: pause_in_force != 0,
                        pause_boundary: parse_boundary(boundary.as_deref())?,
                        created_at: parse_ts(&created_at)?,
                    })
                },
            )
            .collect()
    }

    async fn record_scope(
        &self,
        token: &OwnershipToken,
        goal_id: &GoalId,
        turn_id: &TurnId,
        declaration: &ScopeDeclaration,
        now: Timestamp,
    ) -> Result<ScopeProjection, ScopeWriteError> {
        let mut tx = self
            .pool()
            .begin_with("BEGIN IMMEDIATE")
            .await
            .map_err(StorageError::from)?;
        let row: Option<(String,)> = sqlx::query_as(
            "SELECT goals.state FROM goals \
             JOIN tasks ON tasks.id = goals.task_id \
             WHERE goals.id = ?1 AND goals.task_id = ?2 \
             AND tasks.owner_runtime_id = ?3 AND tasks.owner_epoch = ?4",
        )
        .bind(goal_id.as_str())
        .bind(token.task_id.as_str())
        .bind(token.runtime_id.as_str())
        .bind(token.owner_epoch.get() as i64)
        .fetch_optional(&mut *tx)
        .await
        .map_err(StorageError::from)?;
        let Some((state,)) = row else {
            tx.rollback().await.map_err(StorageError::from)?;
            return match goal_write_miss(self, token, goal_id, false).await {
                Ok(()) => Err(StorageError::InvalidData(format!(
                    "goal {goal_id} rejected the requested requirement change"
                ))
                .into()),
                Err(error) => Err(error.into()),
            };
        };
        if state != GoalState::Running.as_str() {
            tx.rollback().await.map_err(StorageError::from)?;
            return Err(StorageError::InvalidData(format!(
                "goal {goal_id} is settled; a settled goal cannot change its requirements"
            ))
            .into());
        }
        let records = requirement_rows(&mut tx, goal_id.as_str()).await?;
        let history: Vec<AppliedRequirementEvent> =
            records.iter().map(|record| record.event.clone()).collect();
        let current = fold_scope_events(&history)?;
        let projection = apply_scope_declaration(&current, declaration)?;
        let mut seq = records.last().map(|record| record.seq).unwrap_or(0) as i64;
        for event in &projection.events {
            seq += 1;
            insert_requirement_event(&mut tx, goal_id.as_str(), seq, turn_id.as_str(), event, now)
                .await?;
        }
        tx.commit().await.map_err(StorageError::from)?;
        Ok(projection)
    }

    async fn requirement_log(
        &self,
        goal_id: &GoalId,
    ) -> Result<Vec<GoalRequirementRecord>, StorageError> {
        let mut tx = self.pool().begin().await.map_err(StorageError::from)?;
        let records = requirement_rows(&mut tx, goal_id.as_str()).await?;
        tx.commit().await.map_err(StorageError::from)?;
        Ok(records)
    }

    async fn active_capabilities(&self, goal_id: &GoalId) -> Result<Vec<String>, StorageError> {
        if self.get(goal_id).await?.is_none() {
            return Err(StorageError::InvalidData(format!(
                "goal {goal_id} not found"
            )));
        }
        Ok(sqlx::query_scalar(
            "SELECT capability_id FROM goal_capabilities WHERE goal_id = ?1 ORDER BY capability_id",
        )
        .bind(goal_id.as_str())
        .fetch_all(self.pool())
        .await?)
    }

    async fn enable_capability(
        &self,
        token: &OwnershipToken,
        goal_id: &GoalId,
        capability_id: &str,
    ) -> Result<(), OwnershipError> {
        let inserted = sqlx::query(
            "INSERT INTO goal_capabilities (goal_id, capability_id) \
             SELECT goals.id, ?5 FROM goals JOIN tasks ON tasks.id = goals.task_id \
             WHERE goals.id = ?1 AND goals.task_id = ?2 AND goals.state = 'running' \
             AND tasks.owner_runtime_id = ?3 AND tasks.owner_epoch = ?4 \
             ON CONFLICT(goal_id, capability_id) DO UPDATE SET capability_id = excluded.capability_id",
        )
        .bind(goal_id.as_str())
        .bind(token.task_id.as_str())
        .bind(token.runtime_id.as_str())
        .bind(token.owner_epoch.get() as i64)
        .bind(capability_id)
        .execute(self.pool())
        .await
        .map_err(StorageError::from)?;
        if inserted.rows_affected() == 1 {
            return Ok(());
        }
        goal_write_miss(self, token, goal_id, false).await
    }

    async fn get(&self, goal_id: &GoalId) -> Result<Option<GoalRecord>, StorageError> {
        let row: Option<(
            String,
            String,
            String,
            String,
            String,
            Option<String>,
            i64,
            String,
            String,
            Option<String>,
            Option<String>,
        )> = sqlx::query_as(
            "SELECT id, task_id, objective, state, opened_at, settled_at, windows_run, \
             acceptance, amendments, user_paused_at, pause_boundary FROM goals WHERE id = ?1",
        )
        .bind(goal_id.as_str())
        .fetch_optional(self.pool())
        .await?;
        row.map(record_from_row).transpose()
    }

    async fn unfinished(&self) -> Result<Vec<GoalRecord>, StorageError> {
        let rows: Vec<(
            String,
            String,
            String,
            String,
            String,
            Option<String>,
            i64,
            String,
            String,
            Option<String>,
            Option<String>,
        )> = sqlx::query_as(
            "SELECT id, task_id, objective, state, opened_at, settled_at, windows_run, \
             acceptance, amendments, user_paused_at, pause_boundary FROM goals WHERE state = 'running' \
             ORDER BY opened_at DESC",
        )
        .fetch_all(self.pool())
        .await?;
        rows.into_iter().map(record_from_row).collect()
    }

    async fn for_task(&self, task_id: &TaskId) -> Result<Vec<GoalRecord>, StorageError> {
        let rows: Vec<(
            String,
            String,
            String,
            String,
            String,
            Option<String>,
            i64,
            String,
            String,
            Option<String>,
            Option<String>,
        )> = sqlx::query_as(
            "SELECT id, task_id, objective, state, opened_at, settled_at, windows_run, \
             acceptance, amendments, user_paused_at, pause_boundary FROM goals WHERE task_id = ?1 \
             ORDER BY opened_at DESC",
        )
        .bind(task_id.as_str())
        .fetch_all(self.pool())
        .await?;
        rows.into_iter().map(record_from_row).collect()
    }
}

async fn goal_write_miss(
    db: &Database,
    token: &OwnershipToken,
    goal_id: &GoalId,
    settled_is_ok: bool,
) -> Result<(), OwnershipError> {
    let owner = crate::OwnershipStore::current(db, &token.task_id).await?;
    if !owner.as_ref().is_some_and(|owner| {
        owner.runtime.as_ref() == Some(&token.runtime_id) && owner.epoch == token.owner_epoch
    }) {
        return Err(crate::ownership_store::sqlite_stale_error(db, token).await);
    }

    let goal: Option<(String, String)> =
        sqlx::query_as("SELECT task_id, state FROM goals WHERE id = ?1")
            .bind(goal_id.as_str())
            .fetch_optional(db.pool())
            .await
            .map_err(StorageError::from)?;
    match goal {
        Some((task_id, state)) if task_id == token.task_id.as_str() => {
            if settled_is_ok && state == GoalState::Settled.as_str() {
                Ok(())
            } else {
                Err(StorageError::InvalidData(format!(
                    "goal {goal_id} rejected the requested state transition"
                ))
                .into())
            }
        }
        Some(_) => Err(StorageError::InvalidData(format!(
            "goal {goal_id} does not belong to task {}",
            token.task_id
        ))
        .into()),
        None => Err(StorageError::InvalidData(format!("goal {goal_id} not found")).into()),
    }
}

/// One accepted requirement event. `operation` and `kind` are stored as their
/// wire spellings; an unknown spelling is refused rather than decoded as a
/// different obligation.
async fn insert_requirement_event(
    tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    goal_id: &str,
    seq: i64,
    turn_id: &str,
    event: &AppliedRequirementEvent,
    now: Timestamp,
) -> Result<(), StorageError> {
    let verification = event
        .verification
        .as_ref()
        .map(serde_json::to_string)
        .transpose()
        .map_err(|error| {
            StorageError::InvalidData(format!("invalid verification for goal {goal_id}: {error}"))
        })?;
    sqlx::query(
        "INSERT INTO goal_requirement_events \
         (goal_id, seq, turn_id, operation, requirement_id, supersedes, kind, text, target, \
          source_quote, check_text, verification, reason, created_at) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14)",
    )
    .bind(goal_id)
    .bind(seq)
    .bind(turn_id)
    .bind(event.operation.as_str())
    .bind(&event.requirement_id)
    .bind(event.supersedes.as_deref())
    .bind(event.kind.map(|kind| kind.as_str()))
    .bind(event.text.as_deref())
    .bind(event.target.as_deref())
    .bind(&event.source_quote)
    .bind(event.check.as_deref())
    .bind(verification.as_deref())
    .bind(event.reason.as_deref())
    .bind(now.to_rfc3339())
    .execute(&mut **tx)
    .await
    .map_err(StorageError::from)?;
    Ok(())
}

async fn requirement_rows(
    tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    goal_id: &str,
) -> Result<Vec<GoalRequirementRecord>, StorageError> {
    type RequirementRow = (
        i64,
        String,
        String,
        String,
        Option<String>,
        Option<String>,
        Option<String>,
        Option<String>,
        String,
        Option<String>,
        Option<String>,
        Option<String>,
        String,
    );
    let rows: Vec<RequirementRow> = sqlx::query_as(
        "SELECT seq, turn_id, operation, requirement_id, supersedes, kind, text, target, \
         source_quote, check_text, verification, reason, created_at \
         FROM goal_requirement_events WHERE goal_id = ?1 ORDER BY seq",
    )
    .bind(goal_id)
    .fetch_all(&mut **tx)
    .await
    .map_err(StorageError::from)?;
    rows.into_iter()
        .map(
            |(
                seq,
                turn_id,
                operation,
                requirement_id,
                supersedes,
                kind,
                text,
                target,
                source_quote,
                check,
                verification,
                reason,
                created_at,
            )| {
                let operation = ScopeOperationKind::parse(&operation).ok_or_else(|| {
                    StorageError::InvalidData(format!(
                        "unknown requirement operation `{operation}` for goal {goal_id}"
                    ))
                })?;
                let kind = kind
                    .as_deref()
                    .map(|kind| {
                        RequirementKind::parse(kind).ok_or_else(|| {
                            StorageError::InvalidData(format!(
                                "unknown requirement kind `{kind}` for goal {goal_id}"
                            ))
                        })
                    })
                    .transpose()?;
                let verification = verification
                    .as_deref()
                    .map(serde_json::from_str::<RequirementVerification>)
                    .transpose()
                    .map_err(|_| {
                        StorageError::InvalidData(format!(
                            "invalid verification for goal {goal_id}"
                        ))
                    })?;
                Ok(GoalRequirementRecord {
                    seq: seq.max(0) as u32,
                    turn_id,
                    event: AppliedRequirementEvent {
                        operation,
                        requirement_id,
                        supersedes,
                        kind,
                        text,
                        target,
                        source_quote,
                        check,
                        verification,
                        reason,
                    },
                    created_at: parse_ts(&created_at)?,
                })
            },
        )
        .collect()
}

fn record_from_row(
    row: (
        String,
        String,
        String,
        String,
        String,
        Option<String>,
        i64,
        String,
        String,
        Option<String>,
        Option<String>,
    ),
) -> Result<GoalRecord, StorageError> {
    let (
        id,
        task_id,
        objective,
        state,
        opened_at,
        settled_at,
        windows_run,
        acceptance,
        amendments,
        user_paused_at,
        pause_boundary,
    ) = row;
    Ok(GoalRecord {
        id: GoalId::new(id),
        task_id: TaskId::new(task_id),
        objective,
        state: GoalState::parse(&state)
            .ok_or_else(|| StorageError::InvalidData(format!("unknown goal state `{state}`")))?,
        opened_at: parse_ts(&opened_at)?,
        settled_at: settled_at.as_deref().map(parse_ts).transpose()?,
        windows_run: windows_run.max(0) as u32,
        acceptance: parse_list(&acceptance, "acceptance")?,
        amendments: parse_list(&amendments, "amendments")?,
        user_paused_at: user_paused_at.as_deref().map(parse_ts).transpose()?,
        pause_boundary: parse_boundary(pause_boundary.as_deref())?,
    })
}

/// A structured boundary column. A row we cannot decode is refused, never
/// treated as "no boundary": forgetting a user's pause would resume work they
/// withheld authorization for.
fn parse_boundary(raw: Option<&str>) -> Result<Option<PauseBoundary>, StorageError> {
    raw.map(|raw| {
        serde_json::from_str(raw).map_err(|error| {
            StorageError::InvalidData(format!("bad goal pause_boundary JSON `{raw}`: {error}"))
        })
    })
    .transpose()
}

/// A malformed scope column is refused, never defaulted: a goal whose
/// acceptance we cannot read must not silently become a goal with no plan.
fn parse_list(raw: &str, field: &str) -> Result<Vec<String>, StorageError> {
    serde_json::from_str(raw).map_err(|error| {
        StorageError::InvalidData(format!("bad goal {field} JSON `{raw}`: {error}"))
    })
}

fn parse_ts(s: &str) -> Result<Timestamp, StorageError> {
    chrono::DateTime::parse_from_rfc3339(s)
        .map(|t| t.with_timezone(&chrono::Utc))
        .map_err(|e| StorageError::InvalidData(format!("bad timestamp `{s}`: {e}")))
}

/// An in-memory [`GoalStore`] for tests and ephemeral runs, honoring the same
/// contract as the SQLite adapter.
pub struct MemoryGoalStore {
    pub(crate) rows: Mutex<Vec<GoalRecord>>,
    intents: Mutex<Vec<(String, GoalIntentRecord)>>,
    requirements: Mutex<Vec<(String, GoalRequirementRecord)>>,
    capabilities: Mutex<std::collections::BTreeMap<String, std::collections::BTreeSet<String>>>,
    ownership: std::sync::OnceLock<std::sync::Arc<crate::MemoryOwnershipState>>,
}

impl Default for MemoryGoalStore {
    fn default() -> Self {
        Self {
            rows: Mutex::new(Vec::new()),
            intents: Mutex::new(Vec::new()),
            requirements: Mutex::new(Vec::new()),
            capabilities: Mutex::new(std::collections::BTreeMap::new()),
            ownership: std::sync::OnceLock::new(),
        }
    }
}

impl MemoryGoalStore {
    /// An empty store.
    pub fn new() -> Self {
        Self::default()
    }

    /// Couple the goal store to the shared ownership authority used by every
    /// memory fenced store in the same engine.
    pub fn with_ownership(self, state: std::sync::Arc<crate::MemoryOwnershipState>) -> Self {
        let _ = self.ownership.set(state);
        self
    }
}

#[async_trait]
impl GoalStore for MemoryGoalStore {
    async fn open_scoped(
        &self,
        token: &OwnershipToken,
        objective: &str,
        acceptance: &[String],
        requirements: &[RequirementSeed],
        now: Timestamp,
    ) -> Result<GoalId, OwnershipError> {
        let Some(ownership) = self.ownership.get() else {
            return Err(OwnershipError::Storage(StorageError::InvalidData(
                "memory goal store has no ownership authority configured".to_string(),
            )));
        };
        let initial = initial_scope(requirements).map_err(|error| {
            OwnershipError::Storage(StorageError::InvalidData(format!(
                "invalid initial requirements: {error}"
            )))
        })?;
        let id = GoalId::new(leveler_core::new_uuid_string());
        ownership.with_current(token, || {
            self.rows.lock().unwrap().push(GoalRecord {
                id: id.clone(),
                task_id: token.task_id.clone(),
                objective: objective.to_string(),
                state: GoalState::Running,
                opened_at: now,
                settled_at: None,
                windows_run: 0,
                acceptance: acceptance.to_vec(),
                amendments: Vec::new(),
                user_paused_at: None,
                pause_boundary: None,
            });
            let mut records = self.requirements.lock().unwrap();
            for (offset, event) in initial.events.iter().enumerate() {
                records.push((
                    id.to_string(),
                    GoalRequirementRecord {
                        seq: offset as u32 + 1,
                        turn_id: String::new(),
                        event: event.clone(),
                        created_at: now,
                    },
                ));
            }
        })?;
        Ok(id)
    }

    async fn append_amendment(
        &self,
        token: &OwnershipToken,
        goal_id: &GoalId,
        amendment: &str,
    ) -> Result<(), OwnershipError> {
        let amendment = amendment.trim();
        if amendment.is_empty() {
            return Ok(());
        }
        let Some(ownership) = self.ownership.get() else {
            return Err(OwnershipError::Storage(StorageError::InvalidData(
                "memory goal store has no ownership authority configured".to_string(),
            )));
        };
        ownership
            .with_current(token, || {
                let mut rows = self.rows.lock().unwrap();
                let goal = rows
                    .iter_mut()
                    .find(|goal| &goal.id == goal_id && goal.task_id == token.task_id)
                    .ok_or_else(|| {
                        StorageError::InvalidData(format!(
                            "goal {goal_id} not found for task {}",
                            token.task_id
                        ))
                    })?;
                if goal.state != GoalState::Running {
                    return Err(StorageError::InvalidData(format!(
                        "goal {goal_id} is settled; a settled goal cannot gain an amendment"
                    )));
                }
                if !goal.amendments.iter().any(|value| value == amendment) {
                    goal.amendments.push(amendment.to_string());
                }
                Ok::<_, StorageError>(())
            })?
            .map_err(OwnershipError::Storage)
    }

    async fn note_window(
        &self,
        token: &OwnershipToken,
        goal_id: &GoalId,
    ) -> Result<(), OwnershipError> {
        let Some(ownership) = self.ownership.get() else {
            return Err(OwnershipError::Storage(StorageError::InvalidData(
                "memory goal store has no ownership authority configured".to_string(),
            )));
        };
        ownership
            .with_current(token, || {
                let mut rows = self.rows.lock().unwrap();
                let goal = rows
                    .iter_mut()
                    .find(|goal| &goal.id == goal_id && goal.task_id == token.task_id)
                    .ok_or_else(|| {
                        StorageError::InvalidData(format!(
                            "goal {goal_id} not found for task {}",
                            token.task_id
                        ))
                    })?;
                goal.windows_run += 1;
                Ok::<_, StorageError>(())
            })?
            .map_err(OwnershipError::Storage)
    }

    async fn settle(
        &self,
        token: &OwnershipToken,
        goal_id: &GoalId,
        now: Timestamp,
    ) -> Result<(), OwnershipError> {
        let Some(ownership) = self.ownership.get() else {
            return Err(OwnershipError::Storage(StorageError::InvalidData(
                "memory goal store has no ownership authority configured".to_string(),
            )));
        };
        ownership
            .with_current(token, || {
                let mut rows = self.rows.lock().unwrap();
                let goal = rows
                    .iter_mut()
                    .find(|goal| &goal.id == goal_id && goal.task_id == token.task_id)
                    .ok_or_else(|| {
                        StorageError::InvalidData(format!(
                            "goal {goal_id} not found for task {}",
                            token.task_id
                        ))
                    })?;
                if goal.state == GoalState::Running {
                    goal.state = GoalState::Settled;
                    goal.settled_at = Some(now);
                    goal.user_paused_at = None;
                    goal.pause_boundary = None;
                }
                Ok::<_, StorageError>(())
            })?
            .map_err(OwnershipError::Storage)
    }

    async fn reopen(&self, token: &OwnershipToken, goal_id: &GoalId) -> Result<(), OwnershipError> {
        let Some(ownership) = self.ownership.get() else {
            return Err(OwnershipError::Storage(StorageError::InvalidData(
                "memory goal store has no ownership authority configured".to_string(),
            )));
        };
        ownership
            .with_current(token, || {
                let mut rows = self.rows.lock().unwrap();
                let goal = rows
                    .iter_mut()
                    .find(|goal| &goal.id == goal_id && goal.task_id == token.task_id)
                    .ok_or_else(|| {
                        StorageError::InvalidData(format!(
                            "goal {goal_id} not found for task {}",
                            token.task_id
                        ))
                    })?;
                goal.state = GoalState::Running;
                goal.settled_at = None;
                goal.user_paused_at = None;
                goal.pause_boundary = None;
                Ok::<_, StorageError>(())
            })?
            .map_err(OwnershipError::Storage)
    }

    async fn set_user_pause(
        &self,
        token: &OwnershipToken,
        goal_id: &GoalId,
        paused_at: Option<Timestamp>,
    ) -> Result<(), OwnershipError> {
        let Some(ownership) = self.ownership.get() else {
            return Err(OwnershipError::Storage(StorageError::InvalidData(
                "memory goal store has no ownership authority configured".to_string(),
            )));
        };
        ownership
            .with_current(token, || {
                let mut rows = self.rows.lock().unwrap();
                let goal = rows
                    .iter_mut()
                    .find(|goal| &goal.id == goal_id && goal.task_id == token.task_id)
                    .ok_or_else(|| {
                        StorageError::InvalidData(format!(
                            "goal {goal_id} not found for task {}",
                            token.task_id
                        ))
                    })?;
                goal.user_paused_at = paused_at;
                Ok::<_, StorageError>(())
            })?
            .map_err(OwnershipError::Storage)
    }

    async fn record_intent(
        &self,
        token: &OwnershipToken,
        goal_id: &GoalId,
        turn_id: &TurnId,
        declaration: &UserIntentDeclaration,
        projection: &IntentProjection,
        now: Timestamp,
    ) -> Result<(), OwnershipError> {
        let Some(ownership) = self.ownership.get() else {
            return Err(OwnershipError::Storage(StorageError::InvalidData(
                "memory goal store has no ownership authority configured".to_string(),
            )));
        };
        ownership
            .with_current(token, || {
                let mut rows = self.rows.lock().unwrap();
                let goal = rows
                    .iter_mut()
                    .find(|goal| &goal.id == goal_id && goal.task_id == token.task_id)
                    .ok_or_else(|| {
                        StorageError::InvalidData(format!(
                            "goal {goal_id} not found for task {}",
                            token.task_id
                        ))
                    })?;
                if goal.state != GoalState::Running && !projection.cancel {
                    return Err(StorageError::InvalidData(format!(
                        "goal {goal_id} is settled; an intent that does not reopen it cannot apply"
                    )));
                }
                if projection.cancel {
                    goal.state = GoalState::Settled;
                    goal.settled_at = Some(now);
                }
                goal.user_paused_at = projection.pause_in_force.then_some(now);
                goal.pause_boundary = projection.pause_boundary.clone();
                let mut intents = self.intents.lock().unwrap();
                let seq = intents
                    .iter()
                    .filter(|(id, _)| id == goal_id.as_str())
                    .map(|(_, record)| record.seq)
                    .max()
                    .unwrap_or(0)
                    .saturating_add(1);
                intents.push((
                    goal_id.to_string(),
                    GoalIntentRecord {
                        seq,
                        turn_id: turn_id.to_string(),
                        source_quote: declaration.source_quote.clone(),
                        operations: declaration.operations.clone(),
                        pause_in_force: projection.pause_in_force,
                        pause_boundary: projection.pause_boundary.clone(),
                        created_at: now,
                    },
                ));
                Ok::<_, StorageError>(())
            })?
            .map_err(OwnershipError::Storage)
    }

    async fn intent_log(&self, goal_id: &GoalId) -> Result<Vec<GoalIntentRecord>, StorageError> {
        let intents = self.intents.lock().unwrap();
        let mut records: Vec<GoalIntentRecord> = intents
            .iter()
            .filter(|(id, _)| id == goal_id.as_str())
            .map(|(_, record)| record.clone())
            .collect();
        records.sort_by_key(|record| record.seq);
        Ok(records)
    }

    async fn record_scope(
        &self,
        token: &OwnershipToken,
        goal_id: &GoalId,
        turn_id: &TurnId,
        declaration: &ScopeDeclaration,
        now: Timestamp,
    ) -> Result<ScopeProjection, ScopeWriteError> {
        let Some(ownership) = self.ownership.get() else {
            return Err(StorageError::InvalidData(
                "memory goal store has no ownership authority configured".to_string(),
            )
            .into());
        };
        let outcome = ownership.with_current(token, || {
            let rows = self.rows.lock().unwrap();
            let goal = rows
                .iter()
                .find(|goal| &goal.id == goal_id && goal.task_id == token.task_id)
                .ok_or_else(|| {
                    ScopeWriteError::from(StorageError::InvalidData(format!(
                        "goal {goal_id} not found for task {}",
                        token.task_id
                    )))
                })?;
            if goal.state != GoalState::Running {
                return Err(StorageError::InvalidData(format!(
                    "goal {goal_id} is settled; a settled goal cannot change its requirements"
                ))
                .into());
            }
            let history: Vec<AppliedRequirementEvent> = {
                let records = self.requirements.lock().unwrap();
                records
                    .iter()
                    .filter(|(id, _)| id == goal_id.as_str())
                    .map(|(_, record)| record.event.clone())
                    .collect()
            };
            let current = fold_scope_events(&history)?;
            let projection = apply_scope_declaration(&current, declaration)?;
            let mut records = self.requirements.lock().unwrap();
            let mut seq = records
                .iter()
                .filter(|(id, _)| id == goal_id.as_str())
                .map(|(_, record)| record.seq)
                .max()
                .unwrap_or(0);
            for event in &projection.events {
                seq += 1;
                records.push((
                    goal_id.to_string(),
                    GoalRequirementRecord {
                        seq,
                        turn_id: turn_id.to_string(),
                        event: event.clone(),
                        created_at: now,
                    },
                ));
            }
            Ok(projection)
        });
        outcome?
    }

    async fn requirement_log(
        &self,
        goal_id: &GoalId,
    ) -> Result<Vec<GoalRequirementRecord>, StorageError> {
        let records = self.requirements.lock().unwrap();
        let mut rows: Vec<GoalRequirementRecord> = records
            .iter()
            .filter(|(id, _)| id == goal_id.as_str())
            .map(|(_, record)| record.clone())
            .collect();
        rows.sort_by_key(|record| record.seq);
        Ok(rows)
    }

    async fn active_capabilities(&self, goal_id: &GoalId) -> Result<Vec<String>, StorageError> {
        if self.get(goal_id).await?.is_none() {
            return Err(StorageError::InvalidData(format!(
                "goal {goal_id} not found"
            )));
        }
        Ok(self
            .capabilities
            .lock()
            .unwrap()
            .get(goal_id.as_str())
            .map(|ids| ids.iter().cloned().collect())
            .unwrap_or_default())
    }

    async fn enable_capability(
        &self,
        token: &OwnershipToken,
        goal_id: &GoalId,
        capability_id: &str,
    ) -> Result<(), OwnershipError> {
        let ownership = self.ownership.get().ok_or_else(|| {
            StorageError::InvalidData(
                "memory goal store has no ownership authority configured".into(),
            )
        })?;
        ownership
            .with_current(token, || {
                let rows = self.rows.lock().unwrap();
                if !rows.iter().any(|goal| {
                    &goal.id == goal_id
                        && goal.task_id == token.task_id
                        && goal.state == GoalState::Running
                }) {
                    return Err(StorageError::InvalidData(format!(
                        "running goal {goal_id} not found for task {}",
                        token.task_id
                    )));
                }
                self.capabilities
                    .lock()
                    .unwrap()
                    .entry(goal_id.to_string())
                    .or_default()
                    .insert(capability_id.to_owned());
                Ok::<_, StorageError>(())
            })?
            .map_err(OwnershipError::Storage)
    }

    async fn get(&self, goal_id: &GoalId) -> Result<Option<GoalRecord>, StorageError> {
        Ok(self
            .rows
            .lock()
            .unwrap()
            .iter()
            .find(|g| &g.id == goal_id)
            .cloned())
    }

    async fn unfinished(&self) -> Result<Vec<GoalRecord>, StorageError> {
        let mut out: Vec<GoalRecord> = self
            .rows
            .lock()
            .unwrap()
            .iter()
            .filter(|g| g.state == GoalState::Running)
            .cloned()
            .collect();
        out.sort_by(|a, b| b.opened_at.cmp(&a.opened_at));
        Ok(out)
    }

    async fn for_task(&self, task_id: &TaskId) -> Result<Vec<GoalRecord>, StorageError> {
        let mut out: Vec<GoalRecord> = self
            .rows
            .lock()
            .unwrap()
            .iter()
            .filter(|g| &g.task_id == task_id)
            .cloned()
            .collect();
        out.sort_by(|a, b| b.opened_at.cmp(&a.opened_at));
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        MemoryOwnershipState, MemoryOwnershipStore, OwnershipStore, SessionRecord,
        SessionRepository, TaskStore,
    };
    use leveler_core::{OwnerEpoch, RuntimeId, SessionId};

    /// One contract, both implementations. Anything asserted here is a promise
    /// the engine may rely on regardless of which store it was handed.
    async fn assert_goal_store_contract(store: &dyn GoalStore, token: &OwnershipToken) {
        assert!(store.unfinished().await.unwrap().is_empty());

        let goal = store
            .open(token, "add rate limiting to login", leveler_core::now())
            .await
            .unwrap();

        let got = store.get(&goal).await.unwrap().expect("goal exists");
        assert_eq!(got.objective, "add rate limiting to login");
        assert_eq!(got.state, GoalState::Running);
        assert_eq!(got.windows_run, 0);
        assert_eq!(got.settled_at, None);

        assert!(store.active_capabilities(&goal).await.unwrap().is_empty());
        store
            .enable_capability(token, &goal, "memory")
            .await
            .unwrap();
        store
            .enable_capability(token, &goal, "browser")
            .await
            .unwrap();
        store
            .enable_capability(token, &goal, "browser")
            .await
            .unwrap();
        assert_eq!(
            store.active_capabilities(&goal).await.unwrap(),
            vec!["browser", "memory"]
        );

        // A goal that owes work is reported as owed.
        let owed = store.unfinished().await.unwrap();
        assert_eq!(owed.len(), 1);
        assert_eq!(owed[0].id, goal);

        store.note_window(token, &goal).await.unwrap();
        store.note_window(token, &goal).await.unwrap();
        assert_eq!(store.get(&goal).await.unwrap().unwrap().windows_run, 2);

        store
            .settle(token, &goal, leveler_core::now())
            .await
            .unwrap();
        let settled = store.get(&goal).await.unwrap().unwrap();
        assert_eq!(settled.state, GoalState::Settled);
        assert!(settled.settled_at.is_some());
        assert!(
            store
                .enable_capability(token, &goal, "skills")
                .await
                .is_err(),
            "a settled goal cannot load more capabilities"
        );
        assert!(
            store.unfinished().await.unwrap().is_empty(),
            "a settled goal owes nothing"
        );

        // Settling twice keeps the first settled_at: the second caller is not
        // wrong, but it is also not the moment the goal actually settled.
        let first = settled.settled_at;
        store
            .settle(token, &goal, leveler_core::now())
            .await
            .unwrap();
        assert_eq!(store.get(&goal).await.unwrap().unwrap().settled_at, first);

        store.reopen(token, &goal).await.unwrap();
        let reopened = store.get(&goal).await.unwrap().unwrap();
        assert_eq!(reopened.id, goal, "resume preserves exact goal identity");
        assert_eq!(reopened.state, GoalState::Running);
        assert_eq!(reopened.settled_at, None);
        assert_eq!(
            store.active_capabilities(&goal).await.unwrap(),
            vec!["browser", "memory"]
        );
        let next = store
            .open(token, "new goal", leveler_core::now())
            .await
            .unwrap();
        assert!(store.active_capabilities(&next).await.unwrap().is_empty());
        // Retrying the same explicit reopen is idempotent.
        store.reopen(token, &goal).await.unwrap();

        assert_eq!(store.get(&GoalId::new("missing")).await.unwrap(), None);
        assert!(
            store
                .active_capabilities(&GoalId::new("missing"))
                .await
                .is_err()
        );
    }

    /// The reason this table exists rather than columns on `tasks`: a session
    /// stays open and the user runs another goal, so one task hosts many.
    async fn assert_one_task_hosts_many_goals(store: &dyn GoalStore, token: &OwnershipToken) {
        let first = store
            .open(token, "first", leveler_core::now())
            .await
            .unwrap();
        store
            .settle(token, &first, leveler_core::now())
            .await
            .unwrap();
        let second = store
            .open(token, "second", leveler_core::now())
            .await
            .unwrap();

        assert_ne!(first, second, "a second goal is not the first one again");
        let all = store.for_task(&token.task_id).await.unwrap();
        assert_eq!(all.len(), 2, "the first goal's history is not overwritten");
        assert_eq!(
            store.unfinished().await.unwrap().len(),
            1,
            "only the live goal owes work"
        );
    }

    /// Acceptance and user amendments are part of the durable goal contract.
    /// A resumed or compacted window must read the same scope the user
    /// established, and a settled goal may not acquire a new one.
    async fn assert_goal_scope_contract(store: &dyn GoalStore, token: &OwnershipToken) {
        let acceptance = vec![
            "stage-1.txt 存在".to_string(),
            "stage-2.txt 存在".to_string(),
        ];
        let goal = store
            .open_scoped(
                token,
                "complete all three stages",
                &acceptance,
                &[],
                leveler_core::now(),
            )
            .await
            .unwrap();
        let record = store.get(&goal).await.unwrap().expect("goal exists");
        assert_eq!(record.acceptance, acceptance);
        assert!(record.amendments.is_empty());

        let plain = store
            .open(token, "a goal with no candidate plan", leveler_core::now())
            .await
            .unwrap();
        assert!(
            store
                .get(&plain)
                .await
                .unwrap()
                .unwrap()
                .acceptance
                .is_empty()
        );

        store
            .append_amendment(token, &goal, "stage-2.txt 暂时不做")
            .await
            .unwrap();
        // The same user turn replayed after a crash is one fact, not two.
        store
            .append_amendment(token, &goal, "stage-2.txt 暂时不做")
            .await
            .unwrap();
        assert_eq!(
            store.get(&goal).await.unwrap().unwrap().amendments,
            vec!["stage-2.txt 暂时不做".to_string()]
        );

        store
            .settle(token, &goal, leveler_core::now())
            .await
            .unwrap();
        assert!(
            store
                .append_amendment(token, &goal, "after settle")
                .await
                .is_err(),
            "a settled goal cannot gain scope"
        );
        assert_eq!(
            store.get(&goal).await.unwrap().unwrap().amendments,
            vec!["stage-2.txt 暂时不做".to_string()],
            "the refused amendment is not recorded"
        );
    }

    /// Structured intent is the only writer of pause authorization, and its
    /// ordered log is written atomically with the state it produced.
    async fn assert_goal_intent_contract(store: &dyn GoalStore, token: &OwnershipToken) {
        use leveler_lifecycle::{
            StageRef, UserIntentDeclaration, UserIntentOperation, project_intent,
        };

        let goal = store
            .open(token, "continue until the third stage", leveler_core::now())
            .await
            .unwrap();
        let idle = IntentProjection {
            pause_in_force: false,
            pause_boundary: None,
            cancel: false,
            answer_only: false,
            independent_task: false,
        };
        let plan = leveler_lifecycle::PlanState {
            steps: vec![
                leveler_lifecycle::PlanStep {
                    step: "stage one".into(),
                    status: "completed".into(),
                    id: Some("s1".into()),
                    origin: leveler_lifecycle::PlanOrigin::ModelExplicit,
                },
                leveler_lifecycle::PlanStep {
                    step: "stage two".into(),
                    status: "pending".into(),
                    id: Some("s2".into()),
                    origin: leveler_lifecycle::PlanOrigin::ModelExplicit,
                },
            ],
        };
        let pause = UserIntentDeclaration {
            source_quote: "pause after stage one".into(),
            operations: vec![UserIntentOperation::PauseTask {
                boundary: PauseBoundary::AfterStep {
                    step: StageRef {
                        step_id: "s1".into(),
                        label: None,
                    },
                },
            }],
        };
        let projected = project_intent(&pause, &idle, &plan).unwrap();
        let first_turn = TurnId::new("turn-1");
        store
            .record_intent(
                token,
                &goal,
                &first_turn,
                &pause,
                &projected,
                leveler_core::now(),
            )
            .await
            .unwrap();
        let record = store.get(&goal).await.unwrap().unwrap();
        assert!(record.user_paused_at.is_some());
        assert_eq!(
            record.pause_boundary,
            Some(PauseBoundary::AfterStep {
                step: StageRef {
                    step_id: "s1".into(),
                    label: None,
                }
            })
        );

        let release = UserIntentDeclaration {
            source_quote: "keep going, but pause after stage two".into(),
            operations: vec![
                UserIntentOperation::ContinueTask,
                UserIntentOperation::SetPauseBoundary {
                    boundary: PauseBoundary::AfterStep {
                        step: StageRef {
                            step_id: "s2".into(),
                            label: None,
                        },
                    },
                },
            ],
        };
        let projected = project_intent(
            &release,
            &IntentProjection {
                pause_in_force: true,
                pause_boundary: record.pause_boundary.clone(),
                cancel: false,
                answer_only: false,
                independent_task: false,
            },
            &plan,
        )
        .unwrap();
        let second_turn = TurnId::new("turn-2");
        store
            .record_intent(
                token,
                &goal,
                &second_turn,
                &release,
                &projected,
                leveler_core::now(),
            )
            .await
            .unwrap();
        let record = store.get(&goal).await.unwrap().unwrap();
        assert!(record.user_paused_at.is_none(), "the old pause is released");
        assert_eq!(
            record.pause_boundary,
            Some(PauseBoundary::AfterStep {
                step: StageRef {
                    step_id: "s2".into(),
                    label: None,
                }
            }),
            "the new boundary replaces the old one"
        );

        let log = store.intent_log(&goal).await.unwrap();
        assert_eq!(log.len(), 2, "every accepted declaration is history");
        assert_eq!(log[0].seq, 1);
        assert_eq!(log[0].turn_id, "turn-1", "the quoting window is recorded");
        assert_eq!(log[0].source_quote, "pause after stage one");
        assert_eq!(log[1].seq, 2);
        assert_eq!(log[1].turn_id, "turn-2");
        assert_eq!(log[1].operations, release.operations);
        assert!(!log[1].pause_in_force);

        let cancel = UserIntentDeclaration {
            source_quote: "cancel this".into(),
            operations: vec![UserIntentOperation::CancelTask],
        };
        let projected = project_intent(
            &cancel,
            &IntentProjection {
                pause_in_force: false,
                pause_boundary: record.pause_boundary.clone(),
                cancel: false,
                answer_only: false,
                independent_task: false,
            },
            &plan,
        )
        .unwrap();
        store
            .record_intent(
                token,
                &goal,
                &second_turn,
                &cancel,
                &projected,
                leveler_core::now(),
            )
            .await
            .unwrap();
        let cancelled = store.get(&goal).await.unwrap().unwrap();
        assert_eq!(cancelled.state, GoalState::Settled);
        assert!(cancelled.user_paused_at.is_none());
        assert!(cancelled.pause_boundary.is_none());

        let after = UserIntentDeclaration {
            source_quote: "go on".into(),
            operations: vec![UserIntentOperation::ContinueTask],
        };
        let projected = project_intent(
            &after,
            &IntentProjection {
                pause_in_force: false,
                pause_boundary: None,
                cancel: false,
                answer_only: false,
                independent_task: false,
            },
            &plan,
        )
        .unwrap();
        assert!(
            store
                .record_intent(
                    token,
                    &goal,
                    &second_turn,
                    &after,
                    &projected,
                    leveler_core::now(),
                )
                .await
                .is_err(),
            "a settled goal does not accept a continuation without an explicit reopen"
        );
        assert_eq!(store.intent_log(&goal).await.unwrap().len(), 3);
    }

    #[tokio::test]
    async fn memory_store_persists_structured_intent() {
        let state = std::sync::Arc::new(MemoryOwnershipState::new());
        let task = TaskId::new("t1");
        state.register_task(&task);
        let ownership = MemoryOwnershipStore::new(state.clone());
        let token = ownership
            .acquire(
                &task,
                &RuntimeId::new("rt"),
                &leveler_core::BootId::new("test-boot"),
                OwnerEpoch::UNOWNED,
            )
            .await
            .unwrap();
        let store = MemoryGoalStore::new().with_ownership(state);
        assert_goal_intent_contract(&store, &token).await;
    }

    #[tokio::test]
    async fn sqlite_store_persists_structured_intent() {
        let db = Database::connect_in_memory().await.unwrap();
        let (_, token) = seeded_task(&db).await;
        assert_goal_intent_contract(&db, &token).await;
    }

    #[tokio::test]
    async fn memory_store_persists_goal_scope() {
        let state = std::sync::Arc::new(MemoryOwnershipState::new());
        let task = TaskId::new("t1");
        state.register_task(&task);
        let ownership = MemoryOwnershipStore::new(state.clone());
        let token = ownership
            .acquire(
                &task,
                &RuntimeId::new("rt"),
                &leveler_core::BootId::new("test-boot"),
                OwnerEpoch::UNOWNED,
            )
            .await
            .unwrap();
        let store = MemoryGoalStore::new().with_ownership(state);
        assert_goal_scope_contract(&store, &token).await;
    }

    #[tokio::test]
    async fn sqlite_store_persists_goal_scope() {
        let db = Database::connect_in_memory().await.unwrap();
        let (_, token) = seeded_task(&db).await;
        assert_goal_scope_contract(&db, &token).await;
    }

    /// Requirement identity, transition legality and event order are one
    /// durable contract. The current set is always folded from the history, so
    /// a resumed window and a replay see the same obligations.
    async fn assert_goal_requirement_contract(store: &dyn GoalStore, token: &OwnershipToken) {
        use leveler_lifecycle::{
            RequirementKind, RequirementSeed, ScopeDeclaration, ScopeError, ScopeOperation,
        };

        let unusable = store
            .open_scoped(
                token,
                "an invalid seed creates nothing",
                &[],
                &[RequirementSeed {
                    kind: RequirementKind::Content,
                    text: "Verified.".into(),
                    target: None,
                    source_quote: "append Verified.".into(),
                    check: None,
                    verification: None,
                }],
                leveler_core::now(),
            )
            .await;
        assert!(unusable.is_err(), "a content seed needs its file");
        assert!(
            store.unfinished().await.unwrap().is_empty(),
            "an invalid initial scope leaves no goal behind"
        );

        let original_check = RequirementVerification {
            fingerprint: "v1:stage-one".into(),
            registered_by: "call-commit".into(),
        };
        let replacement_check = RequirementVerification {
            fingerprint: "v1:stage-one-with-tests".into(),
            registered_by: "call-amend".into(),
        };
        let goal = store
            .open_scoped(
                token,
                "ship stage one; do not touch the legacy module",
                &[],
                &[
                    RequirementSeed {
                        kind: RequirementKind::Named,
                        text: "stage one shipped".into(),
                        target: None,
                        source_quote: "ship stage one".into(),
                        check: None,
                        verification: Some(original_check.clone()),
                    },
                    RequirementSeed {
                        kind: RequirementKind::Constraint,
                        text: "do not touch the legacy module".into(),
                        target: None,
                        source_quote: "do not touch the legacy module".into(),
                        check: None,
                        verification: None,
                    },
                ],
                leveler_core::now(),
            )
            .await
            .unwrap();
        let items = store.requirements(&goal).await.unwrap();
        assert_eq!(
            items
                .iter()
                .map(|item| (item.id.as_str(), item.kind, item.is_owed()))
                .collect::<Vec<_>>(),
            vec![
                ("R1", RequirementKind::Named, true),
                ("R2", RequirementKind::Constraint, false),
            ]
        );

        assert_eq!(items[0].verification.as_ref(), Some(&original_check));
        let turn = TurnId::new("turn-2");
        let declaration = ScopeDeclaration {
            operations: vec![
                ScopeOperation::Replace {
                    requirement_id: "R1".into(),
                    kind: RequirementKind::Named,
                    text: "stage one shipped with tests".into(),
                    target: None,
                    source_quote: "stage one must also have tests".into(),
                    check: None,
                    verification: Some(replacement_check.clone()),
                    reason: None,
                },
                ScopeOperation::Add {
                    kind: RequirementKind::Named,
                    text: "stage two shipped".into(),
                    target: None,
                    source_quote: "and stage two".into(),
                    check: None,
                    verification: None,
                    reason: None,
                },
            ],
        };
        let projection = store
            .record_scope(token, &goal, &turn, &declaration, leveler_core::now())
            .await
            .unwrap();
        assert_eq!(
            projection
                .events
                .iter()
                .map(|event| event.requirement_id.as_str())
                .collect::<Vec<_>>(),
            vec!["R3", "R4"]
        );
        let items = store.requirements(&goal).await.unwrap();
        assert_eq!(
            items
                .iter()
                .map(|item| (item.id.as_str(), item.state))
                .collect::<Vec<_>>(),
            vec![
                ("R1", leveler_lifecycle::RequirementState::Superseded),
                ("R2", leveler_lifecycle::RequirementState::Active),
                ("R3", leveler_lifecycle::RequirementState::Active),
                ("R4", leveler_lifecycle::RequirementState::Active),
            ]
        );

        assert_eq!(items[0].verification.as_ref(), Some(&original_check));
        assert_eq!(items[2].verification.as_ref(), Some(&replacement_check));
        assert!(items[3].verification.is_none());

        // An unknown identity is refused and writes nothing.
        let refused = store
            .record_scope(
                token,
                &goal,
                &turn,
                &ScopeDeclaration {
                    operations: vec![ScopeOperation::Waive {
                        requirement_id: "R9".into(),
                        source_quote: "drop it".into(),
                        reason: None,
                    }],
                },
                leveler_core::now(),
            )
            .await;
        assert!(matches!(
            refused,
            Err(ScopeWriteError::Scope(
                ScopeError::UnknownRequirement { .. }
            ))
        ));
        assert_eq!(
            store.requirement_log(&goal).await.unwrap().len(),
            4,
            "the refused declaration appended no events"
        );

        // The log is the ordered, turn-attributed history of the set.
        let log = store.requirement_log(&goal).await.unwrap();
        assert_eq!(
            log.iter().map(|record| record.seq).collect::<Vec<_>>(),
            vec![1, 2, 3, 4]
        );
        assert_eq!(log[2].turn_id, "turn-2");
        assert_eq!(log[0].event.source_quote, "ship stage one");
    }

    #[tokio::test]
    async fn memory_store_persists_structured_requirements() {
        let state = std::sync::Arc::new(MemoryOwnershipState::new());
        let task = TaskId::new("t1");
        state.register_task(&task);
        let ownership = MemoryOwnershipStore::new(state.clone());
        let token = ownership
            .acquire(
                &task,
                &RuntimeId::new("rt"),
                &leveler_core::BootId::new("test-boot"),
                OwnerEpoch::UNOWNED,
            )
            .await
            .unwrap();
        let store = MemoryGoalStore::new().with_ownership(state);
        assert_goal_requirement_contract(&store, &token).await;
    }

    #[tokio::test]
    async fn sqlite_store_persists_structured_requirements() {
        let db = Database::connect_in_memory().await.unwrap();
        let (_, token) = seeded_task(&db).await;
        assert_goal_requirement_contract(&db, &token).await;
    }

    #[tokio::test]
    async fn an_amendment_cannot_cross_goal_ownership() {
        let db = Database::connect_in_memory().await.unwrap();
        let (_, first) = seeded_task(&db).await;
        let (_, second) = seeded_task(&db).await;
        let goal = db
            .open(&first, "first task", leveler_core::now())
            .await
            .unwrap();
        assert!(
            db.append_amendment(&second, &goal, "not this task's scope")
                .await
                .is_err()
        );
        assert!(
            db.get(&goal).await.unwrap().unwrap().amendments.is_empty(),
            "the stale token wrote nothing"
        );
    }

    #[tokio::test]
    async fn memory_store_honors_the_contract() {
        let state = std::sync::Arc::new(MemoryOwnershipState::new());
        let task = TaskId::new("t1");
        state.register_task(&task);
        let ownership = MemoryOwnershipStore::new(state.clone());
        let token = ownership
            .acquire(
                &task,
                &RuntimeId::new("rt"),
                &leveler_core::BootId::new("test-boot"),
                OwnerEpoch::UNOWNED,
            )
            .await
            .unwrap();
        let store = MemoryGoalStore::new().with_ownership(state);
        assert_goal_store_contract(&store, &token).await;
    }

    #[tokio::test]
    async fn memory_store_hosts_many_goals_per_task() {
        let state = std::sync::Arc::new(MemoryOwnershipState::new());
        let task = TaskId::new("t1");
        state.register_task(&task);
        let ownership = MemoryOwnershipStore::new(state.clone());
        let token = ownership
            .acquire(
                &task,
                &RuntimeId::new("rt"),
                &leveler_core::BootId::new("test-boot"),
                OwnerEpoch::UNOWNED,
            )
            .await
            .unwrap();
        let store = MemoryGoalStore::new().with_ownership(state);
        assert_one_task_hosts_many_goals(&store, &token).await;
    }

    async fn seeded_task(db: &Database) -> (TaskId, OwnershipToken) {
        let record = SessionRecord::new("/repo", "goal", "mock/m", leveler_core::now());
        SessionRepository::new(db).create(&record).await.unwrap();
        let task = db
            .ensure_for_session(&SessionId::new(record.id), leveler_core::now())
            .await
            .unwrap();
        let token = db
            .acquire(
                &task,
                &RuntimeId::new("rt"),
                &leveler_core::BootId::new("test-boot"),
                OwnerEpoch::UNOWNED,
            )
            .await
            .unwrap();
        (task, token)
    }

    /// A user message that arrives mid-turn is appended to the goal while the
    /// runtime is writing elsewhere (a command receipt, the event log of
    /// another lane). The amendment is a read-then-write: it must take the
    /// write lock at BEGIN and wait for the other writer. A deferred BEGIN
    /// reads first, and the following UPDATE then fails immediately with
    /// SQLITE_BUSY — which surfaced as "persistence error: database is locked"
    /// and killed the whole turn the user had just typed into.
    #[tokio::test]
    async fn an_amendment_waits_for_a_concurrent_writer_instead_of_failing() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sessions.db");
        let db = Database::connect(&path).await.unwrap();
        let (_, token) = seeded_task(&db).await;
        let goal = db
            .open(&token, "ship the feature", leveler_core::now())
            .await
            .unwrap();

        // A second connection, as the runtime's own receipt/log writers are.
        let other = Database::connect(&path).await.unwrap();
        let pool = other.pool().clone();
        let session = SessionId::new(
            sqlx::query_scalar::<_, String>("SELECT id FROM sessions LIMIT 1")
                .fetch_one(&pool)
                .await
                .unwrap(),
        );
        let holder_session = session.clone();
        let holder = tokio::spawn(async move {
            let mut tx = pool.begin_with("BEGIN IMMEDIATE").await.unwrap();
            sqlx::query("UPDATE sessions SET updated_at = updated_at WHERE id = ?1")
                .bind(holder_session.as_str())
                .execute(&mut *tx)
                .await
                .unwrap();
            tokio::time::sleep(std::time::Duration::from_millis(400)).await;
            tx.commit().await.unwrap();
        });
        // Let the holder acquire the write lock before the amendment reads.
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;

        let result = db.append_amendment(&token, &goal, "先不要动配置").await;
        holder.await.unwrap();
        assert!(
            result.is_ok(),
            "a concurrent writer must not abort the user's amendment: {result:?}"
        );
        assert_eq!(
            db.get(&goal).await.unwrap().unwrap().amendments,
            vec!["先不要动配置".to_string()]
        );
    }

    #[tokio::test]
    async fn sqlite_store_honors_the_contract() {
        let db = Database::connect_in_memory().await.unwrap();
        let (_, token) = seeded_task(&db).await;
        assert_goal_store_contract(&db, &token).await;
    }

    #[tokio::test]
    async fn capability_enable_rejects_a_goal_owned_by_another_task() {
        let db = Database::connect_in_memory().await.unwrap();
        let (_, first) = seeded_task(&db).await;
        let (_, second) = seeded_task(&db).await;
        let goal = db
            .open(&first, "first task", leveler_core::now())
            .await
            .unwrap();
        assert!(
            db.enable_capability(&second, &goal, "browser")
                .await
                .is_err()
        );
        assert!(db.active_capabilities(&goal).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn sqlite_store_hosts_many_goals_per_task() {
        let db = Database::connect_in_memory().await.unwrap();
        let (_, token) = seeded_task(&db).await;
        assert_one_task_hosts_many_goals(&db, &token).await;
    }

    /// A goal outlives the process, so it must be readable from a fresh
    /// handle — the whole point of the table.
    #[tokio::test]
    async fn a_goal_is_readable_after_reconnecting() {
        let dir = std::env::temp_dir().join(format!(
            "leveler-goal-store-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("state.db");
        let goal = {
            let db = Database::connect(&path).await.unwrap();
            let (_, token) = seeded_task(&db).await;
            let goal = db
                .open(&token, "survive a restart", leveler_core::now())
                .await
                .unwrap();
            db.enable_capability(&token, &goal, "browser")
                .await
                .unwrap();
            goal
        };
        let db = Database::connect(&path).await.unwrap();
        let owed = db.unfinished().await.unwrap();
        assert_eq!(owed.len(), 1, "the goal must survive the connection");
        assert_eq!(owed[0].id, goal);
        assert_eq!(owed[0].objective, "survive a restart");
        assert_eq!(
            db.active_capabilities(&goal).await.unwrap(),
            vec!["browser"]
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    /// Deleting a session must not leave goals nobody can reach.
    #[tokio::test]
    async fn goals_cascade_with_their_task() {
        let db = Database::connect_in_memory().await.unwrap();
        let record = SessionRecord::new("/repo", "goal", "mock/m", leveler_core::now());
        SessionRepository::new(&db).create(&record).await.unwrap();
        let session = SessionId::new(record.id);
        let task = db
            .ensure_for_session(&session, leveler_core::now())
            .await
            .unwrap();
        let token = db
            .acquire(
                &task,
                &RuntimeId::new("rt"),
                &leveler_core::BootId::new("test-boot"),
                OwnerEpoch::UNOWNED,
            )
            .await
            .unwrap();
        db.open(&token, "doomed", leveler_core::now())
            .await
            .unwrap();

        SessionRepository::new(&db).delete(&session).await.unwrap();
        assert!(
            db.unfinished().await.unwrap().is_empty(),
            "deleting a session must not leave orphan goals owing work forever"
        );
    }

    /// An unreadable state must not silently become "settled" and stop being
    /// reported as owed work.
    #[test]
    fn an_unknown_state_is_refused_not_defaulted() {
        assert_eq!(GoalState::parse("running"), Some(GoalState::Running));
        assert_eq!(GoalState::parse("settled"), Some(GoalState::Settled));
        assert_eq!(GoalState::parse("interrupted"), None);
        assert_eq!(GoalState::parse(""), None);
    }

    async fn assert_stale_open_is_rejected(
        store: &dyn GoalStore,
        stale: &OwnershipToken,
        current: &OwnershipToken,
    ) {
        let result = store
            .open(stale, "must not land", leveler_core::now())
            .await;
        assert!(matches!(result, Err(OwnershipError::Stale { .. })));
        assert!(store.for_task(&stale.task_id).await.unwrap().is_empty());

        let goal = store
            .open(current, "current owner", leveler_core::now())
            .await
            .unwrap();
        store
            .enable_capability(current, &goal, "browser")
            .await
            .unwrap();
        for capability in ["browser", "memory"] {
            assert!(
                matches!(
                    store.enable_capability(stale, &goal, capability).await,
                    Err(OwnershipError::Stale { .. })
                ),
                "even duplicate enables must be fenced"
            );
        }
        assert_eq!(
            store.active_capabilities(&goal).await.unwrap(),
            vec!["browser"]
        );
        assert_eq!(store.for_task(&current.task_id).await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn sqlite_open_is_fenced_by_owner_epoch() {
        let db = Database::connect_in_memory().await.unwrap();
        let (task, stale) = seeded_task(&db).await;
        let current = db
            .acquire(
                &task,
                &RuntimeId::new("rt"),
                &leveler_core::BootId::new("test-boot"),
                stale.owner_epoch,
            )
            .await
            .unwrap();
        assert_stale_open_is_rejected(&db, &stale, &current).await;
    }

    #[tokio::test]
    async fn memory_open_is_fenced_by_owner_epoch() {
        let state = std::sync::Arc::new(MemoryOwnershipState::new());
        let task = TaskId::new("t1");
        state.register_task(&task);
        let ownership = MemoryOwnershipStore::new(state.clone());
        let stale = ownership
            .acquire(
                &task,
                &RuntimeId::new("rt"),
                &leveler_core::BootId::new("test-boot"),
                OwnerEpoch::UNOWNED,
            )
            .await
            .unwrap();
        let current = ownership
            .acquire(
                &task,
                &RuntimeId::new("rt"),
                &leveler_core::BootId::new("test-boot"),
                stale.owner_epoch,
            )
            .await
            .unwrap();
        let store = MemoryGoalStore::new().with_ownership(state);
        assert_stale_open_is_rejected(&store, &stale, &current).await;
    }
}
