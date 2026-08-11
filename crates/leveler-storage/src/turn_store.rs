//! The `TurnStore` port: the narrow turn-persistence seam the engine depends
//! on — starting a turn and finding running turns for recovery.
//!
//! Terminal turn commits are deliberately NOT here: `finish_turn` must land
//! atomically with its canonical event and lives on [`crate::TerminalStore`].
//! The other [`crate::TurnRepository`] methods (`list`, bulk interrupts) are
//! app/test conveniences and stay concrete.

use std::sync::Mutex;

use async_trait::async_trait;

use leveler_core::{SessionId, Timestamp, TurnId};

use crate::{Database, StorageError, TurnRecord, TurnRepository};

/// The engine-facing turn persistence contract.
#[async_trait]
pub trait TurnStore: Send + Sync {
    /// Start a new turn: assign the session's next ordinal, insert the row
    /// with status `running`, and return it.
    async fn start(
        &self,
        session_id: &SessionId,
        kind: &str,
        payload: Option<&str>,
        now: Timestamp,
    ) -> Result<TurnRecord, StorageError>;

    /// Running turns, optionally restricted to one session — the recovery
    /// query behind the reaper. Ordering is stable (per-session ordinal).
    async fn list_running(
        &self,
        session_id: Option<&SessionId>,
    ) -> Result<Vec<TurnRecord>, StorageError>;

    /// Fenced start: like `start`, but atomically guarded on `token` being
    /// the session's task's current ownership (single guarded statement in
    /// SQLite). A stale runtime cannot open new execution turns.
    async fn start_owned(
        &self,
        token: &leveler_core::OwnershipToken,
        session_id: &SessionId,
        kind: &str,
        payload: Option<&str>,
        now: Timestamp,
    ) -> Result<TurnRecord, crate::OwnershipError>;
}

/// The production SQLite adapter: delegates to [`TurnRepository`].
#[async_trait]
impl TurnStore for Database {
    async fn start(
        &self,
        session_id: &SessionId,
        kind: &str,
        payload: Option<&str>,
        now: Timestamp,
    ) -> Result<TurnRecord, StorageError> {
        TurnRepository::new(self)
            .start(session_id, kind, payload, now)
            .await
    }

    async fn list_running(
        &self,
        session_id: Option<&SessionId>,
    ) -> Result<Vec<TurnRecord>, StorageError> {
        TurnRepository::new(self).list_running(session_id).await
    }

    async fn start_owned(
        &self,
        token: &leveler_core::OwnershipToken,
        session_id: &SessionId,
        kind: &str,
        payload: Option<&str>,
        now: Timestamp,
    ) -> Result<TurnRecord, crate::OwnershipError> {
        let id = TurnId::generate().into_inner();
        let payload = payload.map(leveler_core::redact_secrets);
        // Ordinal assignment AND ownership guard inside one INSERT.
        let inserted = sqlx::query(
            "INSERT INTO turns (id, session_id, ordinal, kind, payload, status, created_at) \
             SELECT ?1, ?2, \
                    (SELECT COALESCE(MAX(ordinal), 0) + 1 FROM turns WHERE session_id = ?2), \
                    ?3, ?4, 'running', ?5 \
             WHERE EXISTS (SELECT 1 FROM tasks WHERE session_id = ?2 \
                           AND id = ?6 AND owner_runtime_id = ?7 AND owner_epoch = ?8)",
        )
        .bind(&id)
        .bind(session_id.as_str())
        .bind(kind)
        .bind(payload.as_deref())
        .bind(now.to_rfc3339())
        .bind(token.task_id.as_str())
        .bind(token.runtime_id.as_str())
        .bind(token.owner_epoch.get() as i64)
        .execute(self.pool())
        .await
        .map_err(StorageError::from)?;
        if inserted.rows_affected() != 1 {
            return Err(crate::ownership_store::sqlite_stale_error(self, token).await);
        }
        Ok(sqlx::query_as::<_, TurnRecord>(
            "SELECT id, session_id, ordinal, kind, payload, status, created_at, finished_at \
             FROM turns WHERE id = ?1",
        )
        .bind(&id)
        .fetch_one(self.pool())
        .await
        .map_err(StorageError::from)?)
    }
}

/// An in-memory [`TurnStore`] honoring the same contract (per-session
/// ordinals, `running` status on start, recovery filtering).
#[derive(Default)]
pub struct MemoryTurnStore {
    pub(crate) rows: Mutex<Vec<TurnRecord>>,
    ownership: std::sync::OnceLock<std::sync::Arc<crate::MemoryOwnershipState>>,
}

impl MemoryTurnStore {
    /// An empty store.
    pub fn new() -> Self {
        Self::default()
    }

    /// Couple to the shared ownership authority for `start_owned`.
    pub fn with_ownership(self, state: std::sync::Arc<crate::MemoryOwnershipState>) -> Self {
        let _ = self.ownership.set(state);
        self
    }

    /// Test hook: the status of one turn (the port itself has no per-turn
    /// read; the engine never needs one).
    pub fn status(&self, id: &TurnId) -> Option<String> {
        self.rows
            .lock()
            .unwrap()
            .iter()
            .find(|t| t.id == id.as_str())
            .map(|t| t.status.clone())
    }
}

#[async_trait]
impl TurnStore for MemoryTurnStore {
    async fn start(
        &self,
        session_id: &SessionId,
        kind: &str,
        payload: Option<&str>,
        now: Timestamp,
    ) -> Result<TurnRecord, StorageError> {
        let mut rows = self.rows.lock().unwrap();
        let ordinal = rows
            .iter()
            .filter(|t| t.session_id == session_id.as_str())
            .map(|t| t.ordinal)
            .max()
            .unwrap_or(0)
            + 1;
        let record = TurnRecord {
            id: TurnId::generate().into_inner(),
            session_id: session_id.as_str().to_string(),
            ordinal,
            kind: kind.to_string(),
            payload: payload.map(leveler_core::redact_secrets),
            status: "running".to_string(),
            created_at: now.to_rfc3339(),
            finished_at: None,
        };
        rows.push(record.clone());
        Ok(record)
    }

    async fn list_running(
        &self,
        session_id: Option<&SessionId>,
    ) -> Result<Vec<TurnRecord>, StorageError> {
        let rows = self.rows.lock().unwrap();
        let mut out: Vec<TurnRecord> = rows
            .iter()
            .filter(|t| {
                t.status == "running"
                    && t.finished_at.is_none()
                    && session_id.is_none_or(|s| t.session_id == s.as_str())
            })
            .cloned()
            .collect();
        out.sort_by(|a, b| {
            a.session_id
                .cmp(&b.session_id)
                .then(a.ordinal.cmp(&b.ordinal))
        });
        Ok(out)
    }

    async fn start_owned(
        &self,
        token: &leveler_core::OwnershipToken,
        session_id: &SessionId,
        kind: &str,
        payload: Option<&str>,
        now: Timestamp,
    ) -> Result<TurnRecord, crate::OwnershipError> {
        let Some(ownership) = self.ownership.get() else {
            return Err(crate::OwnershipError::Storage(StorageError::InvalidData(
                "memory turn store has no ownership authority configured".to_string(),
            )));
        };
        ownership.with_current(token, || {
            let mut rows = self.rows.lock().unwrap();
            let ordinal = rows
                .iter()
                .filter(|t| t.session_id == session_id.as_str())
                .map(|t| t.ordinal)
                .max()
                .unwrap_or(0)
                + 1;
            let record = TurnRecord {
                id: TurnId::generate().into_inner(),
                session_id: session_id.as_str().to_string(),
                ordinal,
                kind: kind.to_string(),
                payload: payload.map(leveler_core::redact_secrets),
                status: "running".to_string(),
                created_at: now.to_rfc3339(),
                finished_at: None,
            };
            rows.push(record.clone());
            record
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{SessionRecord, SessionRepository};

    /// One contract against both implementations: ordinals are per-session
    /// and gapless from 1, new turns are `running`, and the recovery query
    /// filters by session.
    async fn assert_turn_store_contract(
        store: &dyn TurnStore,
        session_a: &SessionId,
        session_b: &SessionId,
    ) {
        let t1 = store
            .start(session_a, "user", None, leveler_core::now())
            .await
            .unwrap();
        let t2 = store
            .start(session_a, "chat", Some(r#"{"k":1}"#), leveler_core::now())
            .await
            .unwrap();
        let other = store
            .start(session_b, "user", None, leveler_core::now())
            .await
            .unwrap();
        assert_eq!((t1.ordinal, t2.ordinal), (1, 2), "per-session ordinals");
        assert_eq!(other.ordinal, 1, "sessions do not share ordinal space");
        assert_eq!(t1.status, "running");

        let running_a = store.list_running(Some(session_a)).await.unwrap();
        assert_eq!(
            running_a.iter().map(|t| t.id.as_str()).collect::<Vec<_>>(),
            vec![t1.id.as_str(), t2.id.as_str()]
        );
        let all = store.list_running(None).await.unwrap();
        assert_eq!(all.len(), 3);
    }

    #[tokio::test]
    async fn sqlite_store_honors_the_contract() {
        let db = Database::connect_in_memory().await.unwrap();
        let a = SessionRecord::new("/repo", "a", "mock/m", leveler_core::now());
        let b = SessionRecord::new("/repo", "b", "mock/m", leveler_core::now());
        SessionRepository::new(&db).create(&a).await.unwrap();
        SessionRepository::new(&db).create(&b).await.unwrap();
        assert_turn_store_contract(&db, &SessionId::new(a.id), &SessionId::new(b.id)).await;
    }

    #[tokio::test]
    async fn memory_store_honors_the_contract() {
        let store = MemoryTurnStore::new();
        assert_turn_store_contract(&store, &SessionId::generate(), &SessionId::generate()).await;
    }
}
