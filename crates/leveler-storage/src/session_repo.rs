//! Session repository with create/list/get/delete operations so the CLI's
//! `sessions` commands work against a real table; the orchestrator fills in the
//! rest in later phases.

use std::str::FromStr;

use serde::{Deserialize, Serialize};

use leveler_core::{SessionId, Timestamp};
use leveler_lifecycle::{AgentState, SessionStatus, TaskOutcome, UnknownVariant};

use crate::database::{Database, StorageError};

impl From<UnknownVariant> for StorageError {
    fn from(err: UnknownVariant) -> Self {
        StorageError::InvalidData(err.to_string())
    }
}

/// A persisted session row. Status/state/outcome are typed lifecycle vocabulary,
/// not free strings — the repository encodes them to text on write and decodes
/// on read, raising `InvalidData` on any unknown persisted value.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionRecord {
    pub id: String,
    pub repository: String,
    pub goal: String,
    pub status: SessionStatus,
    pub model: String,
    pub state: AgentState,
    pub created_at: String,
    pub updated_at: String,
    /// Collaboration mode wire value: chat | plan | goal.
    pub collaboration: String,
    /// Work profile wire value: economy | balanced | delivery.
    pub work_profile: String,
}

/// The raw text row as stored; decoded into a typed [`SessionRecord`].
#[derive(sqlx::FromRow)]
struct SessionRow {
    id: String,
    repository: String,
    goal: String,
    status: String,
    model: String,
    state: String,
    created_at: String,
    updated_at: String,
    collaboration: String,
    work_profile: String,
}

impl SessionRow {
    fn decode(self) -> Result<SessionRecord, StorageError> {
        Ok(SessionRecord {
            id: self.id,
            repository: self.repository,
            goal: self.goal,
            status: SessionStatus::from_str(&self.status)?,
            model: self.model,
            state: AgentState::from_str(&self.state)?,
            created_at: self.created_at,
            updated_at: self.updated_at,
            collaboration: self.collaboration,
            work_profile: self.work_profile,
        })
    }
}

/// Read/write access to the `sessions` table.
pub struct SessionRepository<'a> {
    db: &'a Database,
}

impl<'a> SessionRepository<'a> {
    pub fn new(db: &'a Database) -> Self {
        Self { db }
    }

    /// Insert a new session.
    #[allow(clippy::too_many_arguments)]
    pub async fn create(&self, record: &SessionRecord) -> Result<(), StorageError> {
        // Explicit mode/sandbox/kind: migration 0003 DEFAULT is still the obsolete
        // `workspace_write` string; never depend on that for new rows.
        sqlx::query(
            "INSERT INTO sessions \
             (id, repository, goal, status, model, state, created_at, updated_at, \
              collaboration, work_profile, mode, sandbox, kind) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, 'assisted', 0, 'direct')",
        )
        .bind(&record.id)
        .bind(&record.repository)
        .bind(&record.goal)
        .bind(record.status.as_str())
        .bind(&record.model)
        .bind(record.state.as_str())
        .bind(&record.created_at)
        .bind(&record.updated_at)
        .bind(&record.collaboration)
        .bind(&record.work_profile)
        .execute(self.db.pool())
        .await?;
        Ok(())
    }

    /// List sessions, most recent first.
    pub async fn list(&self) -> Result<Vec<SessionRecord>, StorageError> {
        let rows = sqlx::query_as::<_, SessionRow>(
            "SELECT id, repository, goal, status, model, state, created_at, updated_at, \
             collaboration, work_profile \
             FROM sessions ORDER BY created_at DESC",
        )
        .fetch_all(self.db.pool())
        .await?;
        rows.into_iter().map(SessionRow::decode).collect()
    }

    /// Fetch one session by id.
    pub async fn get(&self, id: &SessionId) -> Result<Option<SessionRecord>, StorageError> {
        let row = sqlx::query_as::<_, SessionRow>(
            "SELECT id, repository, goal, status, model, state, created_at, updated_at, \
             collaboration, work_profile \
             FROM sessions WHERE id = ?1",
        )
        .bind(id.as_str())
        .fetch_optional(self.db.pool())
        .await?;
        row.map(SessionRow::decode).transpose()
    }

    /// Persist product axes (collaboration × work profile).
    pub async fn set_axes(
        &self,
        id: &SessionId,
        collaboration: &str,
        work_profile: &str,
        now: Timestamp,
    ) -> Result<(), StorageError> {
        sqlx::query(
            "UPDATE sessions SET collaboration = ?2, work_profile = ?3, updated_at = ?4 \
             WHERE id = ?1",
        )
        .bind(id.as_str())
        .bind(collaboration)
        .bind(work_profile)
        .bind(now.to_rfc3339())
        .execute(self.db.pool())
        .await?;
        Ok(())
    }

    /// Read product axes for a session.
    pub async fn axes(&self, id: &SessionId) -> Result<Option<(String, String)>, StorageError> {
        let row: Option<(String, String)> =
            sqlx::query_as("SELECT collaboration, work_profile FROM sessions WHERE id = ?1")
                .bind(id.as_str())
                .fetch_optional(self.db.pool())
                .await?;
        Ok(row)
    }

    /// Update a session's operational status and agent state, bumping
    /// `updated_at`.
    pub async fn update_status(
        &self,
        id: &SessionId,
        status: SessionStatus,
        state: AgentState,
        now: Timestamp,
    ) -> Result<(), StorageError> {
        sqlx::query("UPDATE sessions SET status = ?2, state = ?3, updated_at = ?4 WHERE id = ?1")
            .bind(id.as_str())
            .bind(status.as_str())
            .bind(state.as_str())
            .bind(now.to_rfc3339())
            .execute(self.db.pool())
            .await?;
        Ok(())
    }

    /// Overwrite the goal/title text. Used to name a placeholder interactive
    /// session after its first real message; does not bump `updated_at` (the
    /// message's own turn does that).
    pub async fn update_goal(&self, id: &SessionId, goal: &str) -> Result<(), StorageError> {
        sqlx::query("UPDATE sessions SET goal = ?2 WHERE id = ?1")
            .bind(id.as_str())
            .bind(goal)
            .execute(self.db.pool())
            .await?;
        Ok(())
    }

    /// Persist the model selected for subsequent turns in this session.
    pub async fn update_model(
        &self,
        id: &SessionId,
        model: &str,
        now: Timestamp,
    ) -> Result<(), StorageError> {
        sqlx::query("UPDATE sessions SET model = ?2, updated_at = ?3 WHERE id = ?1")
            .bind(id.as_str())
            .bind(model)
            .bind(now.to_rfc3339())
            .execute(self.db.pool())
            .await?;
        Ok(())
    }

    /// Persist how the session executes (0003 columns). Set at creation time
    /// so resume never has to guess mode/sandbox/kind.
    pub async fn set_execution(
        &self,
        id: &SessionId,
        mode: &str,
        sandbox: bool,
        kind: &str,
        now: Timestamp,
    ) -> Result<(), StorageError> {
        sqlx::query(
            "UPDATE sessions SET mode = ?2, sandbox = ?3, kind = ?4, updated_at = ?5 \
             WHERE id = ?1",
        )
        .bind(id.as_str())
        .bind(mode)
        .bind(sandbox)
        .bind(kind)
        .bind(now.to_rfc3339())
        .execute(self.db.pool())
        .await?;
        Ok(())
    }

    /// Read back `(mode, sandbox, kind, outcome)`. `kind` stays a string
    /// (owned by the engine's `ExecutionKind`, parsed there); the terminal
    /// `outcome` is decoded to the typed lifecycle vocabulary.
    pub async fn execution(
        &self,
        id: &SessionId,
    ) -> Result<Option<(String, bool, String, Option<TaskOutcome>)>, StorageError> {
        let row: Option<(String, bool, String, Option<String>)> =
            sqlx::query_as("SELECT mode, sandbox, kind, outcome FROM sessions WHERE id = ?1")
                .bind(id.as_str())
                .fetch_optional(self.db.pool())
                .await?;
        match row {
            None => Ok(None),
            Some((mode, sandbox, kind, outcome)) => {
                let outcome = outcome.map(|s| TaskOutcome::from_str(&s)).transpose()?;
                Ok(Some((mode, sandbox, kind, outcome)))
            }
        }
    }

    /// Persist the terminal outcome.
    pub async fn set_outcome(
        &self,
        id: &SessionId,
        outcome: TaskOutcome,
        now: Timestamp,
    ) -> Result<(), StorageError> {
        sqlx::query("UPDATE sessions SET outcome = ?2, updated_at = ?3 WHERE id = ?1")
            .bind(id.as_str())
            .bind(outcome.as_str())
            .bind(now.to_rfc3339())
            .execute(self.db.pool())
            .await?;
        Ok(())
    }

    /// Delete a session and all its cascaded rows. Returns whether a row matched.
    pub async fn delete(&self, id: &SessionId) -> Result<bool, StorageError> {
        let result = sqlx::query("DELETE FROM sessions WHERE id = ?1")
            .bind(id.as_str())
            .execute(self.db.pool())
            .await?;
        Ok(result.rows_affected() > 0)
    }
}

impl SessionRecord {
    /// Build a fresh record from the essentials, stamping timestamps.
    pub fn new(
        repository: impl Into<String>,
        goal: impl Into<String>,
        model: impl Into<String>,
        now: Timestamp,
    ) -> Self {
        let ts = now.to_rfc3339();
        Self {
            id: SessionId::generate().into_inner(),
            repository: repository.into(),
            goal: goal.into(),
            status: SessionStatus::Created,
            model: model.into(),
            state: AgentState::Understand,
            created_at: ts.clone(),
            updated_at: ts,
            collaboration: "goal".into(),
            work_profile: "balanced".into(),
        }
    }

    /// Set product axes before insert.
    pub fn with_axes(mut self, collaboration: &str, work_profile: &str) -> Self {
        self.collaboration = collaboration.to_string();
        self.work_profile = work_profile.to_string();
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn create_list_get_delete_roundtrip() {
        let db = Database::connect_in_memory().await.unwrap();
        let repo = SessionRepository::new(&db);
        assert!(repo.list().await.unwrap().is_empty());

        let record = SessionRecord::new(
            "/repo",
            "fix bug",
            "deepseek/deepseek-v4-pro",
            leveler_core::now(),
        )
        .with_axes("goal", "balanced");
        repo.create(&record).await.unwrap();

        let listed = repo.list().await.unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].goal, "fix bug");

        let id = SessionId::new(record.id.clone());
        assert!(repo.get(&id).await.unwrap().is_some());
        assert!(repo.delete(&id).await.unwrap());
        assert!(repo.get(&id).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn update_goal_overwrites_title_without_touching_updated_at() {
        let db = Database::connect_in_memory().await.unwrap();
        let repo = SessionRepository::new(&db);
        let record = SessionRecord::new("/repo", "interactive session", "m", leveler_core::now());
        repo.create(&record).await.unwrap();
        let id = SessionId::new(record.id.clone());

        repo.update_goal(&id, "帮我修复登录超时").await.unwrap();
        let got = repo.get(&id).await.unwrap().unwrap();
        assert_eq!(got.goal, "帮我修复登录超时");
        assert_eq!(
            got.updated_at, record.updated_at,
            "retitle must not reorder the session list"
        );
    }

    #[tokio::test]
    async fn update_status_mutates_status_state_and_timestamp() {
        let db = Database::connect_in_memory().await.unwrap();
        let repo = SessionRepository::new(&db);

        let record = SessionRecord::new("/repo", "fix bug", "openai/gpt-4o", leveler_core::now());
        repo.create(&record).await.unwrap();

        let id = SessionId::new(record.id.clone());
        let updated = leveler_core::now();
        repo.update_status(&id, SessionStatus::Running, AgentState::Plan, updated)
            .await
            .unwrap();

        let fetched = repo.get(&id).await.unwrap().expect("record exists");
        assert_eq!(fetched.status, SessionStatus::Running);
        assert_eq!(fetched.state, AgentState::Plan);
        assert_eq!(fetched.updated_at, updated.to_rfc3339());
    }

    #[test]
    fn session_record_new_stamps_defaults() {
        let now = leveler_core::now();
        let record = SessionRecord::new("/repo", "goal", "model", now);
        assert_eq!(record.repository, "/repo");
        assert_eq!(record.goal, "goal");
        assert_eq!(record.model, "model");
        assert_eq!(record.status, SessionStatus::Created);
        assert_eq!(record.state, AgentState::Understand);
        assert_eq!(record.created_at, now.to_rfc3339());
        assert_eq!(record.updated_at, now.to_rfc3339());
        assert!(!record.id.is_empty());
    }

    #[tokio::test]
    async fn known_string_values_decode_and_unknown_is_corruption() {
        let db = Database::connect_in_memory().await.unwrap();
        let repo = SessionRepository::new(&db);
        let record = SessionRecord::new("/repo", "goal", "m", leveler_core::now());
        repo.create(&record).await.unwrap();
        let id = SessionId::new(record.id);

        // A persisted status string still decodes to the typed enum.
        sqlx::query("UPDATE sessions SET status = 'completed', state = 'review' WHERE id = ?1")
            .bind(id.as_str())
            .execute(db.pool())
            .await
            .unwrap();
        let fetched = repo.get(&id).await.unwrap().unwrap();
        assert_eq!(fetched.status, SessionStatus::Completed);
        assert_eq!(fetched.state, AgentState::Review);

        // An unknown persisted value is a named corruption error, never a
        // silent default.
        sqlx::query("UPDATE sessions SET status = 'bogus' WHERE id = ?1")
            .bind(id.as_str())
            .execute(db.pool())
            .await
            .unwrap();
        let err = repo.get(&id).await.unwrap_err();
        assert!(
            matches!(&err, StorageError::InvalidData(m) if m.contains("bogus")),
            "unexpected error: {err}"
        );
    }

    #[tokio::test]
    async fn execution_config_and_outcome_roundtrip() {
        let db = Database::connect_in_memory().await.unwrap();
        let repo = SessionRepository::new(&db);
        let record = SessionRecord::new("/repo", "goal", "m", leveler_core::now());
        repo.create(&record).await.unwrap();
        let id = SessionId::new(record.id);

        // Fresh rows use the assisted profile (migration 0012/0013),
        // not the obsolete workspace_write string from 0003.
        let (mode, sandbox, kind, outcome) = repo.execution(&id).await.unwrap().unwrap();
        assert_eq!(
            (mode.as_str(), sandbox, kind.as_str(), outcome),
            ("assisted", false, "direct", None)
        );

        repo.set_execution(&id, "full_access", true, "orchestrate", leveler_core::now())
            .await
            .unwrap();
        repo.set_outcome(&id, TaskOutcome::CompletedUnverified, leveler_core::now())
            .await
            .unwrap();

        let (mode, sandbox, kind, outcome) = repo.execution(&id).await.unwrap().unwrap();
        assert_eq!(mode, "full_access");
        assert!(sandbox);
        assert_eq!(kind, "orchestrate");
        assert_eq!(outcome, Some(TaskOutcome::CompletedUnverified));
    }

    #[tokio::test]
    async fn product_axes_persist_and_default() {
        let db = Database::connect_in_memory().await.unwrap();
        let repo = SessionRepository::new(&db);
        let record = SessionRecord::new("/repo", "goal", "m", leveler_core::now())
            .with_axes("goal", "delivery");
        repo.create(&record).await.unwrap();
        let id = SessionId::new(record.id.clone());
        let loaded = repo.get(&id).await.unwrap().unwrap();
        assert_eq!(loaded.collaboration, "goal");
        assert_eq!(loaded.work_profile, "delivery");
        repo.set_axes(&id, "chat", "economy", leveler_core::now())
            .await
            .unwrap();
        let (c, w) = repo.axes(&id).await.unwrap().unwrap();
        assert_eq!((c.as_str(), w.as_str()), ("chat", "economy"));
    }
}
