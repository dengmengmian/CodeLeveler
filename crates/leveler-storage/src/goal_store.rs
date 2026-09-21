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

use leveler_core::{GoalId, OwnershipToken, TaskId, Timestamp};

use crate::{Database, OwnershipError, StorageError};

/// Does this goal still owe work?
///
/// Two states on purpose. There is deliberately no `Interrupted`: a goal left
/// `Running` by a process that died IS the interrupted one, and task ownership
/// already records whether a live runtime is driving it. A third state would
/// be a second writer for a fact ownership already holds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GoalState {
    /// Work is owed. Whether anyone is currently driving it is an ownership
    /// question, not a goal-state question.
    Running,
    /// No further work is owed. How it went is `sessions.outcome`.
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
}

/// Access to durable goals.
#[async_trait]
pub trait GoalStore: Send + Sync {
    /// Open a goal against the task currently owned by `token`.
    ///
    /// The ownership assertion and insert are one atomic write. A stale token
    /// creates no goal and returns a typed ownership error.
    async fn open(
        &self,
        token: &OwnershipToken,
        objective: &str,
        now: Timestamp,
    ) -> Result<GoalId, OwnershipError>;

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

    /// Reopen this exact settled goal for an explicit continuation. This does
    /// not create or select a goal by objective text; ownership and task
    /// identity are checked atomically with the state transition.
    async fn reopen(&self, token: &OwnershipToken, goal_id: &GoalId) -> Result<(), OwnershipError>;

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
    async fn open(
        &self,
        token: &OwnershipToken,
        objective: &str,
        now: Timestamp,
    ) -> Result<GoalId, OwnershipError> {
        let id = GoalId::new(leveler_core::new_uuid_string());
        let inserted = sqlx::query(
            "INSERT INTO goals (id, task_id, objective, state, opened_at, windows_run) \
             SELECT ?1, id, ?5, ?6, ?7, 0 FROM tasks \
             WHERE id = ?2 AND owner_runtime_id = ?3 AND owner_epoch = ?4",
        )
        .bind(id.as_str())
        .bind(token.task_id.as_str())
        .bind(token.runtime_id.as_str())
        .bind(token.owner_epoch.get() as i64)
        .bind(objective)
        .bind(GoalState::Running.as_str())
        .bind(now.to_rfc3339())
        .execute(self.pool())
        .await
        .map_err(StorageError::from)?;
        if inserted.rows_affected() == 1 {
            return Ok(id);
        }
        Err(crate::ownership_store::sqlite_stale_error(self, token).await)
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
            "UPDATE goals SET state = ?2, settled_at = ?3 \
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
            "UPDATE goals SET state = 'running', settled_at = NULL \
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

    async fn get(&self, goal_id: &GoalId) -> Result<Option<GoalRecord>, StorageError> {
        let row: Option<(String, String, String, String, String, Option<String>, i64)> =
            sqlx::query_as(
                "SELECT id, task_id, objective, state, opened_at, settled_at, windows_run \
                 FROM goals WHERE id = ?1",
            )
            .bind(goal_id.as_str())
            .fetch_optional(self.pool())
            .await?;
        row.map(record_from_row).transpose()
    }

    async fn unfinished(&self) -> Result<Vec<GoalRecord>, StorageError> {
        let rows: Vec<(String, String, String, String, String, Option<String>, i64)> =
            sqlx::query_as(
                "SELECT id, task_id, objective, state, opened_at, settled_at, windows_run \
                 FROM goals WHERE state = 'running' ORDER BY opened_at DESC",
            )
            .fetch_all(self.pool())
            .await?;
        rows.into_iter().map(record_from_row).collect()
    }

    async fn for_task(&self, task_id: &TaskId) -> Result<Vec<GoalRecord>, StorageError> {
        let rows: Vec<(String, String, String, String, String, Option<String>, i64)> =
            sqlx::query_as(
                "SELECT id, task_id, objective, state, opened_at, settled_at, windows_run \
                 FROM goals WHERE task_id = ?1 ORDER BY opened_at DESC",
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

fn record_from_row(
    row: (String, String, String, String, String, Option<String>, i64),
) -> Result<GoalRecord, StorageError> {
    let (id, task_id, objective, state, opened_at, settled_at, windows_run) = row;
    Ok(GoalRecord {
        id: GoalId::new(id),
        task_id: TaskId::new(task_id),
        objective,
        state: GoalState::parse(&state)
            .ok_or_else(|| StorageError::InvalidData(format!("unknown goal state `{state}`")))?,
        opened_at: parse_ts(&opened_at)?,
        settled_at: settled_at.as_deref().map(parse_ts).transpose()?,
        windows_run: windows_run.max(0) as u32,
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
    ownership: std::sync::OnceLock<std::sync::Arc<crate::MemoryOwnershipState>>,
}

impl Default for MemoryGoalStore {
    fn default() -> Self {
        Self {
            rows: Mutex::new(Vec::new()),
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
    async fn open(
        &self,
        token: &OwnershipToken,
        objective: &str,
        now: Timestamp,
    ) -> Result<GoalId, OwnershipError> {
        let Some(ownership) = self.ownership.get() else {
            return Err(OwnershipError::Storage(StorageError::InvalidData(
                "memory goal store has no ownership authority configured".to_string(),
            )));
        };
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
            });
        })?;
        Ok(id)
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
        // Retrying the same explicit reopen is idempotent.
        store.reopen(token, &goal).await.unwrap();

        assert_eq!(store.get(&GoalId::new("missing")).await.unwrap(), None);
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

    #[tokio::test]
    async fn sqlite_store_honors_the_contract() {
        let db = Database::connect_in_memory().await.unwrap();
        let (_, token) = seeded_task(&db).await;
        assert_goal_store_contract(&db, &token).await;
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
            db.open(&token, "survive a restart", leveler_core::now())
                .await
                .unwrap()
        };
        let db = Database::connect(&path).await.unwrap();
        let owed = db.unfinished().await.unwrap();
        assert_eq!(owed.len(), 1, "the goal must survive the connection");
        assert_eq!(owed[0].id, goal);
        assert_eq!(owed[0].objective, "survive a restart");
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

        store
            .open(current, "current owner", leveler_core::now())
            .await
            .unwrap();
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
