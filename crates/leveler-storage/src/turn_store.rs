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

    /// Orphan turns plus finished turns whose durable task cancellation still
    /// awaits its task terminal. Recovery must survive a crash between these
    /// two distinct lifecycle commits.
    async fn list_recovery_candidates(
        &self,
        session_id: Option<&SessionId>,
    ) -> Result<Vec<TurnRecord>, StorageError>;

    /// All turns for one session in ordinal order. Resume uses the durable
    /// turn payload to recover the exact work lineage that failed.
    async fn list_for_session(
        &self,
        session_id: &SessionId,
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

    async fn list_recovery_candidates(
        &self,
        session_id: Option<&SessionId>,
    ) -> Result<Vec<TurnRecord>, StorageError> {
        Ok(sqlx::query_as::<_, TurnRecord>(
            "SELECT t.* FROM turns t WHERE (?1 IS NULL OR t.session_id = ?1) AND \
             (t.status = 'running' OR (t.ordinal = (SELECT MAX(ordinal) FROM turns \
             WHERE session_id = t.session_id) AND EXISTS(SELECT 1 FROM events e \
             WHERE e.session_id = t.session_id AND e.turn_id = t.id \
             AND e.type = 'task_cancel_requested' AND NOT EXISTS(SELECT 1 FROM events terminal \
             WHERE terminal.session_id = e.session_id AND terminal.type = 'task_finished' \
             AND terminal.sequence > e.sequence)))) ORDER BY t.session_id, t.ordinal",
        )
        .bind(session_id.map(SessionId::as_str))
        .fetch_all(self.pool())
        .await?)
    }

    async fn list_for_session(
        &self,
        session_id: &SessionId,
    ) -> Result<Vec<TurnRecord>, StorageError> {
        TurnRepository::new(self).list(session_id).await
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
        let payload = payload
            .map(|p| crate::redact_json_payload_for_session("turn", p, Some(session_id.as_str())))
            .transpose()
            .map_err(crate::OwnershipError::Storage)?;
        // Ordinal assignment AND ownership guard inside one INSERT.
        let inserted = sqlx::query(
            "INSERT INTO turns (id, session_id, ordinal, kind, payload, status, created_at, \
                                owner_boot_id) \
             SELECT ?1, ?2, \
                    (SELECT COALESCE(MAX(ordinal), 0) + 1 FROM turns WHERE session_id = ?2), \
                    ?3, ?4, 'running', ?5, ?9 \
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
        .bind(token.boot_id.as_str())
        .execute(self.pool())
        .await
        .map_err(StorageError::from)?;
        if inserted.rows_affected() != 1 {
            return Err(crate::ownership_store::sqlite_stale_error(self, token).await);
        }
        Ok(sqlx::query_as::<_, TurnRecord>(
            "SELECT id, session_id, ordinal, kind, payload, status, created_at, finished_at, owner_boot_id \
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
    events: std::sync::OnceLock<std::sync::Arc<crate::MemoryEventStore>>,
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

    /// Couple to the canonical event log for durable cancellation recovery.
    pub fn with_events(self, events: std::sync::Arc<crate::MemoryEventStore>) -> Self {
        let _ = self.events.set(events);
        self
    }

    pub(crate) fn connect_events(&self, events: std::sync::Arc<crate::MemoryEventStore>) {
        let _ = self.events.set(events);
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
        let payload = payload
            .map(|p| crate::redact_json_payload_for_session("turn", p, Some(session_id.as_str())))
            .transpose()?;
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
            payload,
            status: "running".to_string(),
            created_at: now.to_rfc3339(),
            finished_at: None,
            owner_boot_id: None,
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

    async fn list_recovery_candidates(
        &self,
        session_id: Option<&SessionId>,
    ) -> Result<Vec<TurnRecord>, StorageError> {
        let events = self
            .events
            .get()
            .ok_or_else(|| {
                StorageError::InvalidData(
                    "memory recovery requires the canonical event store".to_string(),
                )
            })?
            .recovery_events();
        let rows = self.rows.lock().unwrap();
        let mut candidates: Vec<_> = rows
            .iter()
            .filter(|turn| {
                session_id.is_none_or(|session| turn.session_id == session.as_str())
                    && (turn.status == "running"
                        || (rows
                            .iter()
                            .filter(|row| row.session_id == turn.session_id)
                            .all(|row| row.ordinal <= turn.ordinal)
                            && events.iter().any(|event| {
                                event.session_id == turn.session_id
                                    && event.turn_id.as_deref() == Some(turn.id.as_str())
                                    && event.event_type == "task_cancel_requested"
                                    && !events.iter().any(|terminal| {
                                        terminal.session_id == event.session_id
                                            && terminal.event_type == "task_finished"
                                            && terminal.sequence > event.sequence
                                    })
                            })))
            })
            .cloned()
            .collect();
        candidates.sort_by(|a, b| {
            a.session_id
                .cmp(&b.session_id)
                .then(a.ordinal.cmp(&b.ordinal))
        });
        Ok(candidates)
    }

    async fn list_for_session(
        &self,
        session_id: &SessionId,
    ) -> Result<Vec<TurnRecord>, StorageError> {
        let mut rows: Vec<_> = self
            .rows
            .lock()
            .unwrap()
            .iter()
            .filter(|row| row.session_id == session_id.as_str())
            .cloned()
            .collect();
        rows.sort_by_key(|row| row.ordinal);
        Ok(rows)
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
        let payload = payload
            .map(|p| crate::redact_json_payload_for_session("turn", p, Some(session_id.as_str())))
            .transpose()
            .map_err(crate::OwnershipError::Storage)?;
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
                payload: payload.clone(),
                status: "running".to_string(),
                created_at: now.to_rfc3339(),
                finished_at: None,
                owner_boot_id: Some(token.boot_id.as_str().to_string()),
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

    /// A turn is `running` and owned by a boot in the same insert, and keeps
    /// that boot as provenance once it ends.
    #[tokio::test]
    async fn an_owned_turn_records_its_boot_from_start_through_its_terminal() {
        use crate::{OwnershipStore, TaskStore, TerminalStore};

        let db = Database::connect_in_memory().await.unwrap();
        let record = SessionRecord::new("/repo", "a", "mock/m", leveler_core::now());
        SessionRepository::new(&db).create(&record).await.unwrap();
        let session = SessionId::new(record.id);
        let task = TaskStore::ensure_for_session(&db, &session, leveler_core::now())
            .await
            .unwrap();
        let boot = leveler_core::BootId::new("boot-1");
        let token = OwnershipStore::acquire(
            &db,
            &task,
            &leveler_core::RuntimeId::new("rt"),
            &boot,
            leveler_core::OwnerEpoch::UNOWNED,
        )
        .await
        .unwrap();

        let turn = TurnStore::start_owned(&db, &token, &session, "user", None, leveler_core::now())
            .await
            .unwrap();
        assert_eq!(turn.status, "running");
        assert_eq!(turn.owner_boot_id.as_deref(), Some("boot-1"));

        TerminalStore::finish_turn_owned(
            &db,
            &token,
            &session,
            &TurnId::new(turn.id.clone()),
            "turn_finished",
            "{}",
            leveler_lifecycle::TurnOutcome::Completed,
            leveler_core::now(),
        )
        .await
        .unwrap();
        let ended = TurnRepository::new(&db).list(&session).await.unwrap();
        assert_eq!(ended[0].status, "completed");
        assert_eq!(ended[0].owner_boot_id.as_deref(), Some("boot-1"));
    }
    async fn assert_pending_cancel_recovery_contract(
        turns: &dyn TurnStore,
        events: &dyn crate::EventStore,
        terminal: &dyn crate::TerminalStore,
        sessions: &dyn crate::SessionStore,
    ) {
        let record = crate::SessionRecord::new("/repo", "goal", "mock/m", leveler_core::now());
        sessions.create(&record).await.unwrap();
        let session = SessionId::new(record.id);
        let turn = turns
            .start(&session, "user", None, leveler_core::now())
            .await
            .unwrap();
        events
            .append(
                &session,
                Some(&TurnId::new(turn.id.clone())),
                "task_cancel_requested",
                &serde_json::json!({"type":"task_cancel_requested", "payload":{
                    "task_id":session.as_str(), "turn_id":turn.id, "boot_id":"dead", "owner_epoch":1
                }})
                .to_string(),
                leveler_core::now(),
            )
            .await
            .unwrap();
        terminal
            .finish_turn(
                &session,
                &TurnId::new(turn.id.clone()),
                "turn_finished",
                "{}",
                leveler_lifecycle::TurnOutcome::Interrupted,
                leveler_core::now(),
            )
            .await
            .unwrap();
        assert!(turns.list_running(Some(&session)).await.unwrap().is_empty());
        assert_eq!(
            turns
                .list_recovery_candidates(Some(&session))
                .await
                .unwrap()[0]
                .id,
            turn.id,
            "a task intent survives its already committed turn interruption"
        );
        terminal
            .finish_task(
                &session,
                "task_finished",
                "{}",
                leveler_lifecycle::TaskOutcome::Cancelled,
                leveler_lifecycle::SessionStatus::Cancelled,
                leveler_lifecycle::AgentState::Cancelled,
                leveler_core::now(),
            )
            .await
            .unwrap();
        assert!(
            turns
                .list_recovery_candidates(Some(&session))
                .await
                .unwrap()
                .is_empty(),
            "a committed task terminal ends cancellation recovery"
        );
        let new = turns
            .start(&session, "user", None, leveler_core::now())
            .await
            .unwrap();
        let candidates = turns
            .list_recovery_candidates(Some(&session))
            .await
            .unwrap();
        assert_eq!(candidates.len(), 1);
        assert_eq!(
            candidates[0].id, new.id,
            "an old intent cannot recover over a new turn"
        );
    }

    #[tokio::test]
    async fn sqlite_pending_cancel_recovers_after_turn_terminal() {
        let db = Database::connect_in_memory().await.unwrap();
        assert_pending_cancel_recovery_contract(&db, &db, &db, &db).await;
    }

    #[tokio::test]
    async fn memory_pending_cancel_recovers_after_turn_terminal() {
        let sessions = std::sync::Arc::new(crate::MemorySessionStore::new());
        let turns = std::sync::Arc::new(MemoryTurnStore::new());
        let events = std::sync::Arc::new(crate::MemoryEventStore::new());
        let terminal =
            crate::MemoryTerminalStore::new(sessions.clone(), turns.clone(), events.clone());
        assert_pending_cancel_recovery_contract(
            turns.as_ref(),
            events.as_ref(),
            &terminal,
            sessions.as_ref(),
        )
        .await;
    }

    #[tokio::test]
    async fn memory_recovery_without_event_authority_fails_explicitly() {
        assert!(
            MemoryTurnStore::new()
                .list_recovery_candidates(None)
                .await
                .is_err()
        );
    }
}
