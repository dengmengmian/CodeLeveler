//! The `SessionStore` port: the narrow session-persistence seam the engine
//! depends on.
//!
//! Same pattern as [`crate::EventStore`] / [`crate::TaskStore`]: storage owns
//! the port and its SQLite adapter, and a [`MemorySessionStore`] exercises the
//! identical contract without SQLite. Deliberately narrow — exactly the four
//! operations `TaskEngine` performs (create, execution config, the Running
//! transition, execution read-back). Everything else on
//! [`crate::SessionRepository`] (list/get/archive/rename/…) is app/CLI/Web
//! convenience and stays concrete.
//!
//! Terminal lifecycle writes are NOT here: they must commit atomically with
//! the canonical event and live on [`crate::TerminalStore`].

use std::sync::Mutex;

use async_trait::async_trait;

use leveler_core::{SessionId, Timestamp};
use leveler_lifecycle::{AgentState, SessionStatus, TaskOutcome};

use crate::{Database, SessionRecord, SessionRepository, StorageError};

/// The engine-facing session persistence contract.
#[async_trait]
pub trait SessionStore: Send + Sync {
    /// Insert a new session row.
    async fn create(&self, record: &SessionRecord) -> Result<(), StorageError>;

    /// Persist how the session executes (mode/sandbox/kind), so resume never
    /// guesses.
    async fn set_execution(
        &self,
        id: &SessionId,
        mode: &str,
        sandbox: bool,
        kind: &str,
        now: Timestamp,
    ) -> Result<(), StorageError>;

    /// Non-terminal operational transition (the engine's `mark_running`).
    /// Terminal transitions go through `TerminalStore` atomically instead.
    async fn update_status(
        &self,
        id: &SessionId,
        status: SessionStatus,
        state: AgentState,
        now: Timestamp,
    ) -> Result<(), StorageError>;

    /// Read back `(mode, sandbox, kind, outcome)`; `None` when the session
    /// does not exist.
    async fn execution(
        &self,
        id: &SessionId,
    ) -> Result<Option<(String, bool, String, Option<TaskOutcome>)>, StorageError>;

    /// Fenced [`Self::update_status`]: guard and update in one statement, so
    /// a stale runtime cannot flip the operational lifecycle.
    async fn update_status_owned(
        &self,
        token: &leveler_core::OwnershipToken,
        id: &SessionId,
        status: SessionStatus,
        state: AgentState,
        now: Timestamp,
    ) -> Result<(), crate::OwnershipError>;
}

/// The production SQLite adapter: delegates to [`SessionRepository`].
#[async_trait]
impl SessionStore for Database {
    async fn create(&self, record: &SessionRecord) -> Result<(), StorageError> {
        SessionRepository::new(self).create(record).await
    }

    async fn set_execution(
        &self,
        id: &SessionId,
        mode: &str,
        sandbox: bool,
        kind: &str,
        now: Timestamp,
    ) -> Result<(), StorageError> {
        SessionRepository::new(self)
            .set_execution(id, mode, sandbox, kind, now)
            .await
    }

    async fn update_status(
        &self,
        id: &SessionId,
        status: SessionStatus,
        state: AgentState,
        now: Timestamp,
    ) -> Result<(), StorageError> {
        SessionRepository::new(self)
            .update_status(id, status, state, now)
            .await
    }

    async fn execution(
        &self,
        id: &SessionId,
    ) -> Result<Option<(String, bool, String, Option<TaskOutcome>)>, StorageError> {
        SessionRepository::new(self).execution(id).await
    }

    async fn update_status_owned(
        &self,
        token: &leveler_core::OwnershipToken,
        id: &SessionId,
        status: SessionStatus,
        state: AgentState,
        now: Timestamp,
    ) -> Result<(), crate::OwnershipError> {
        let updated = sqlx::query(
            "UPDATE sessions SET status = ?2, state = ?3, updated_at = ?4 \
             WHERE id = ?1 AND EXISTS (SELECT 1 FROM tasks WHERE session_id = ?1 \
                 AND id = ?5 AND owner_runtime_id = ?6 AND owner_epoch = ?7)",
        )
        .bind(id.as_str())
        .bind(status.as_str())
        .bind(state.as_str())
        .bind(now.to_rfc3339())
        .bind(token.task_id.as_str())
        .bind(token.runtime_id.as_str())
        .bind(token.owner_epoch.get() as i64)
        .execute(self.pool())
        .await
        .map_err(StorageError::from)?;
        if updated.rows_affected() != 1 {
            return Err(crate::ownership_store::sqlite_stale_error(self, token).await);
        }
        Ok(())
    }
}

/// One in-memory session row (only what the port can observe or mutate).
#[derive(Debug, Clone)]
pub(crate) struct MemorySession {
    pub(crate) status: SessionStatus,
    pub(crate) state: AgentState,
    pub(crate) mode: String,
    pub(crate) sandbox: bool,
    pub(crate) kind: String,
    pub(crate) outcome: Option<TaskOutcome>,
}

/// An in-memory [`SessionStore`] honoring the same contract as the SQLite
/// adapter. New rows get the same execution defaults the SQLite `create`
/// stamps explicitly (`assisted`, no sandbox, `direct`).
#[derive(Default)]
pub struct MemorySessionStore {
    pub(crate) rows: Mutex<std::collections::HashMap<String, MemorySession>>,
    ownership: std::sync::OnceLock<std::sync::Arc<crate::MemoryOwnershipState>>,
}

impl MemorySessionStore {
    /// An empty store.
    pub fn new() -> Self {
        Self::default()
    }

    /// Couple to the shared ownership authority for fenced updates.
    pub fn with_ownership(self, state: std::sync::Arc<crate::MemoryOwnershipState>) -> Self {
        let _ = self.ownership.set(state);
        self
    }

    /// Test hook: the lifecycle columns of one session, for contract
    /// assertions that the port itself deliberately does not expose.
    pub fn lifecycle(&self, id: &SessionId) -> Option<(SessionStatus, AgentState)> {
        self.rows
            .lock()
            .unwrap()
            .get(id.as_str())
            .map(|s| (s.status, s.state))
    }
}

#[async_trait]
impl SessionStore for MemorySessionStore {
    async fn create(&self, record: &SessionRecord) -> Result<(), StorageError> {
        let mut rows = self.rows.lock().unwrap();
        if rows.contains_key(&record.id) {
            return Err(StorageError::InvalidData(format!(
                "session {} already exists",
                record.id
            )));
        }
        rows.insert(
            record.id.clone(),
            MemorySession {
                status: record.status,
                state: record.state,
                mode: "assisted".to_string(),
                sandbox: false,
                kind: "direct".to_string(),
                outcome: None,
            },
        );
        Ok(())
    }

    async fn set_execution(
        &self,
        id: &SessionId,
        mode: &str,
        sandbox: bool,
        kind: &str,
        _now: Timestamp,
    ) -> Result<(), StorageError> {
        if let Some(session) = self.rows.lock().unwrap().get_mut(id.as_str()) {
            session.mode = mode.to_string();
            session.sandbox = sandbox;
            session.kind = kind.to_string();
        }
        Ok(())
    }

    async fn update_status(
        &self,
        id: &SessionId,
        status: SessionStatus,
        state: AgentState,
        _now: Timestamp,
    ) -> Result<(), StorageError> {
        if let Some(session) = self.rows.lock().unwrap().get_mut(id.as_str()) {
            session.status = status;
            session.state = state;
        }
        Ok(())
    }

    async fn execution(
        &self,
        id: &SessionId,
    ) -> Result<Option<(String, bool, String, Option<TaskOutcome>)>, StorageError> {
        Ok(self
            .rows
            .lock()
            .unwrap()
            .get(id.as_str())
            .map(|s| (s.mode.clone(), s.sandbox, s.kind.clone(), s.outcome)))
    }

    async fn update_status_owned(
        &self,
        token: &leveler_core::OwnershipToken,
        id: &SessionId,
        status: SessionStatus,
        state: AgentState,
        _now: Timestamp,
    ) -> Result<(), crate::OwnershipError> {
        let Some(ownership) = self.ownership.get() else {
            return Err(crate::OwnershipError::Storage(StorageError::InvalidData(
                "memory session store has no ownership authority configured".to_string(),
            )));
        };
        ownership.with_current(token, || {
            if let Some(session) = self.rows.lock().unwrap().get_mut(id.as_str()) {
                session.status = status;
                session.state = state;
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// One contract against both implementations: fresh rows read back the
    /// explicit execution defaults, set_execution round-trips, and a missing
    /// session is `None`, never an error.
    async fn assert_session_store_contract(store: &dyn SessionStore) {
        let record = SessionRecord::new("/repo", "goal", "mock/m", leveler_core::now());
        let id = SessionId::new(record.id.clone());
        assert_eq!(store.execution(&id).await.unwrap(), None);

        store.create(&record).await.unwrap();
        let (mode, sandbox, kind, outcome) = store.execution(&id).await.unwrap().unwrap();
        assert_eq!(
            (mode.as_str(), sandbox, kind.as_str(), outcome),
            ("assisted", false, "direct", None),
            "fresh rows carry the explicit execution defaults"
        );

        store
            .set_execution(&id, "full_access", true, "parallel", leveler_core::now())
            .await
            .unwrap();
        let (mode, sandbox, kind, outcome) = store.execution(&id).await.unwrap().unwrap();
        assert_eq!(
            (mode.as_str(), sandbox, kind.as_str(), outcome),
            ("full_access", true, "parallel", None)
        );

        store
            .update_status(
                &id,
                SessionStatus::Running,
                AgentState::Execute,
                leveler_core::now(),
            )
            .await
            .unwrap();
        // The port exposes no status read (the engine never reads it back);
        // callers verify through their implementation's own view below.
    }

    #[tokio::test]
    async fn sqlite_store_honors_the_contract() {
        let db = Database::connect_in_memory().await.unwrap();
        assert_session_store_contract(&db).await;
    }

    #[tokio::test]
    async fn memory_store_honors_the_contract() {
        let store = MemorySessionStore::new();
        assert_session_store_contract(&store).await;
    }

    #[tokio::test]
    async fn memory_update_status_lands_on_the_row() {
        let store = MemorySessionStore::new();
        let record = SessionRecord::new("/repo", "goal", "mock/m", leveler_core::now());
        let id = SessionId::new(record.id.clone());
        store.create(&record).await.unwrap();
        store
            .update_status(
                &id,
                SessionStatus::Running,
                AgentState::Execute,
                leveler_core::now(),
            )
            .await
            .unwrap();
        assert_eq!(
            store.lifecycle(&id),
            Some((SessionStatus::Running, AgentState::Execute))
        );
    }
}
