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

use leveler_core::{GoalId, TaskId, Timestamp};

use crate::{Database, StorageError};

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

/// Identity access to durable goals.
#[async_trait]
pub trait GoalStore: Send + Sync {
    /// Open a goal against a task. Returns its new id.
    async fn open(
        &self,
        task_id: &TaskId,
        objective: &str,
        now: Timestamp,
    ) -> Result<GoalId, StorageError>;

    /// Note that a work window ran. Idempotency is the caller's business —
    /// this counts calls, because a window that ran twice really is two
    /// windows.
    async fn note_window(&self, goal_id: &GoalId) -> Result<(), StorageError>;

    /// Mark a goal as owing no further work. Settling twice is a no-op rather
    /// than an error: the terminal path can be reached from more than one
    /// place, and the second caller is not wrong.
    async fn settle(&self, goal_id: &GoalId, now: Timestamp) -> Result<(), StorageError>;

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
        task_id: &TaskId,
        objective: &str,
        now: Timestamp,
    ) -> Result<GoalId, StorageError> {
        let id = GoalId::new(leveler_core::new_uuid_string());
        sqlx::query(
            "INSERT INTO goals (id, task_id, objective, state, opened_at, windows_run) \
             VALUES (?1, ?2, ?3, ?4, ?5, 0)",
        )
        .bind(id.as_str())
        .bind(task_id.as_str())
        .bind(objective)
        .bind(GoalState::Running.as_str())
        .bind(now.to_rfc3339())
        .execute(self.pool())
        .await?;
        Ok(id)
    }

    async fn note_window(&self, goal_id: &GoalId) -> Result<(), StorageError> {
        sqlx::query("UPDATE goals SET windows_run = windows_run + 1 WHERE id = ?1")
            .bind(goal_id.as_str())
            .execute(self.pool())
            .await?;
        Ok(())
    }

    async fn settle(&self, goal_id: &GoalId, now: Timestamp) -> Result<(), StorageError> {
        // `state = 'running'` in the predicate makes a second settle a no-op
        // and keeps the first settled_at, which is the one that is true.
        sqlx::query(
            "UPDATE goals SET state = ?2, settled_at = ?3 WHERE id = ?1 AND state = 'running'",
        )
        .bind(goal_id.as_str())
        .bind(GoalState::Settled.as_str())
        .bind(now.to_rfc3339())
        .execute(self.pool())
        .await?;
        Ok(())
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
#[derive(Default)]
pub struct MemoryGoalStore {
    rows: Mutex<Vec<GoalRecord>>,
}

impl MemoryGoalStore {
    /// An empty store.
    pub fn new() -> Self {
        Self::default()
    }
}

#[async_trait]
impl GoalStore for MemoryGoalStore {
    async fn open(
        &self,
        task_id: &TaskId,
        objective: &str,
        now: Timestamp,
    ) -> Result<GoalId, StorageError> {
        let id = GoalId::new(leveler_core::new_uuid_string());
        self.rows.lock().unwrap().push(GoalRecord {
            id: id.clone(),
            task_id: task_id.clone(),
            objective: objective.to_string(),
            state: GoalState::Running,
            opened_at: now,
            settled_at: None,
            windows_run: 0,
        });
        Ok(id)
    }

    async fn note_window(&self, goal_id: &GoalId) -> Result<(), StorageError> {
        let mut rows = self.rows.lock().unwrap();
        if let Some(g) = rows.iter_mut().find(|g| &g.id == goal_id) {
            g.windows_run += 1;
        }
        Ok(())
    }

    async fn settle(&self, goal_id: &GoalId, now: Timestamp) -> Result<(), StorageError> {
        let mut rows = self.rows.lock().unwrap();
        if let Some(g) = rows
            .iter_mut()
            .find(|g| &g.id == goal_id && g.state == GoalState::Running)
        {
            g.state = GoalState::Settled;
            g.settled_at = Some(now);
        }
        Ok(())
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
    use crate::{SessionRecord, SessionRepository, TaskStore};
    use leveler_core::SessionId;

    /// One contract, both implementations. Anything asserted here is a promise
    /// the engine may rely on regardless of which store it was handed.
    async fn assert_goal_store_contract(store: &dyn GoalStore, task: &TaskId) {
        assert!(store.unfinished().await.unwrap().is_empty());

        let goal = store
            .open(task, "add rate limiting to login", leveler_core::now())
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

        store.note_window(&goal).await.unwrap();
        store.note_window(&goal).await.unwrap();
        assert_eq!(store.get(&goal).await.unwrap().unwrap().windows_run, 2);

        store.settle(&goal, leveler_core::now()).await.unwrap();
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
        store.settle(&goal, leveler_core::now()).await.unwrap();
        assert_eq!(store.get(&goal).await.unwrap().unwrap().settled_at, first);

        assert_eq!(store.get(&GoalId::new("missing")).await.unwrap(), None);
    }

    /// The reason this table exists rather than columns on `tasks`: a session
    /// stays open and the user runs another goal, so one task hosts many.
    async fn assert_one_task_hosts_many_goals(store: &dyn GoalStore, task: &TaskId) {
        let first = store
            .open(task, "first", leveler_core::now())
            .await
            .unwrap();
        store.settle(&first, leveler_core::now()).await.unwrap();
        let second = store
            .open(task, "second", leveler_core::now())
            .await
            .unwrap();

        assert_ne!(first, second, "a second goal is not the first one again");
        let all = store.for_task(task).await.unwrap();
        assert_eq!(all.len(), 2, "the first goal's history is not overwritten");
        assert_eq!(
            store.unfinished().await.unwrap().len(),
            1,
            "only the live goal owes work"
        );
    }

    #[tokio::test]
    async fn memory_store_honors_the_contract() {
        let store = MemoryGoalStore::new();
        assert_goal_store_contract(&store, &TaskId::new("t1")).await;
    }

    #[tokio::test]
    async fn memory_store_hosts_many_goals_per_task() {
        let store = MemoryGoalStore::new();
        assert_one_task_hosts_many_goals(&store, &TaskId::new("t1")).await;
    }

    async fn seeded_task(db: &Database) -> TaskId {
        let record = SessionRecord::new("/repo", "goal", "mock/m", leveler_core::now());
        SessionRepository::new(db).create(&record).await.unwrap();
        db.ensure_for_session(&SessionId::new(record.id), leveler_core::now())
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn sqlite_store_honors_the_contract() {
        let db = Database::connect_in_memory().await.unwrap();
        let task = seeded_task(&db).await;
        assert_goal_store_contract(&db, &task).await;
    }

    #[tokio::test]
    async fn sqlite_store_hosts_many_goals_per_task() {
        let db = Database::connect_in_memory().await.unwrap();
        let task = seeded_task(&db).await;
        assert_one_task_hosts_many_goals(&db, &task).await;
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
            let task = seeded_task(&db).await;
            db.open(&task, "survive a restart", leveler_core::now())
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
        db.open(&task, "doomed", leveler_core::now()).await.unwrap();

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
}
