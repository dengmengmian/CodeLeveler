//! The `EventStore` port: the narrow append-only-log seam the engine's
//! `EventLog` depends on, so it is decoupled from a concrete SQLite `Database`.
//!
//! The trait lives here (storage owns the port and its SQLite adapter); the
//! engine depends on it through the existing `engine -> storage` edge — no
//! back-edge. A [`MemoryEventStore`] lets the engine's log be exercised without
//! starting SQLite, and both implementations honor one contract (gapless
//! per-session sequences in insertion order).

use std::sync::Mutex;

use async_trait::async_trait;

use leveler_core::{SessionId, Timestamp, TurnId};

use crate::event_repo::EVENT_SCHEMA_VERSION;
use crate::{Database, EventRecord, EventRepository, StorageError};

/// Structure-aware redaction + validation for one event payload (R007 F2).
fn redact_validated(
    event_type: &str,
    payload: &str,
    session: &SessionId,
) -> Result<String, StorageError> {
    crate::redact_json_payload_for_session(
        &format!("event (type '{event_type}')"),
        payload,
        Some(session.as_str()),
    )
}

/// Append/load access to the canonical event log, abstracted over the backing
/// store. Deliberately narrow: only what `EventLog` and incremental readers
/// need.
#[async_trait]
pub trait EventStore: Send + Sync {
    /// Append one event, assigning the next per-session sequence atomically,
    /// and return the persisted record.
    async fn append(
        &self,
        session_id: &SessionId,
        turn_id: Option<&TurnId>,
        event_type: &str,
        payload: &str,
        now: Timestamp,
    ) -> Result<EventRecord, StorageError>;

    /// All events of a session in sequence order.
    async fn load(&self, session_id: &SessionId) -> Result<Vec<EventRecord>, StorageError>;

    /// Events with `sequence > after`, in sequence order (incremental pull).
    async fn load_after(
        &self,
        session_id: &SessionId,
        after: i64,
    ) -> Result<Vec<EventRecord>, StorageError>;

    /// The newest event of `event_type`, optionally scoped to one turn.
    /// Backed by an index in the SQLite adapter — callers may treat this as a
    /// cheap single-row lookup, never a log scan.
    async fn load_last_by_type(
        &self,
        session_id: &SessionId,
        event_type: &str,
        turn_id: Option<&TurnId>,
    ) -> Result<Option<EventRecord>, StorageError>;

    /// Inclusive sequence window `[from_seq, to_seq]`, ordered by sequence.
    /// Callers must bound the range; this is not a full-session dump.
    ///
    /// Default filters [`Self::load`] for test doubles. Production stores
    /// override with a range query.
    async fn load_window(
        &self,
        session_id: &SessionId,
        from_seq: i64,
        to_seq: i64,
    ) -> Result<Vec<EventRecord>, StorageError> {
        let mut rows = self.load(session_id).await?;
        rows.retain(|e| e.sequence >= from_seq && e.sequence <= to_seq);
        Ok(rows)
    }

    /// Highest sequence for the session, if any events exist.
    async fn latest_sequence(&self, session_id: &SessionId) -> Result<Option<i64>, StorageError> {
        Ok(self
            .load(session_id)
            .await?
            .into_iter()
            .map(|e| e.sequence)
            .max())
    }

    /// Per-type counts for overview (canonical log, no payload scan).
    async fn count_by_type(
        &self,
        session_id: &SessionId,
    ) -> Result<Vec<(String, i64)>, StorageError> {
        let rows = self.load(session_id).await?;
        let mut order: Vec<String> = Vec::new();
        let mut counts: std::collections::HashMap<String, i64> = std::collections::HashMap::new();
        for e in rows {
            let entry = counts.entry(e.event_type.clone()).or_insert_with(|| {
                order.push(e.event_type.clone());
                0
            });
            *entry += 1;
        }
        Ok(order
            .into_iter()
            .map(|t| {
                let n = counts[&t];
                (t, n)
            })
            .collect())
    }

    /// Events whose `type` is in `types`, in sequence order.
    ///
    /// Used by observatory session-wide aggregates so callers do not load the
    /// full session log. Default filters [`Self::load`]; production stores
    /// override with an indexed type query.
    async fn load_by_types(
        &self,
        session_id: &SessionId,
        types: &[&str],
    ) -> Result<Vec<EventRecord>, StorageError> {
        if types.is_empty() {
            return Ok(Vec::new());
        }
        let mut rows = self.load(session_id).await?;
        rows.retain(|e| types.iter().any(|t| e.event_type == *t));
        Ok(rows)
    }

    /// Fenced append: like `append`, but atomically guarded on `token` being
    /// the session's task's CURRENT ownership. The check and the insert are
    /// one atomic persistence step (single guarded statement in SQLite) — a
    /// stale token stores nothing and gets a typed
    /// [`crate::OwnershipError::Stale`]. This is how a runtime that lost its
    /// task becomes unable to extend the canonical log.
    async fn append_owned(
        &self,
        token: &leveler_core::OwnershipToken,
        session_id: &SessionId,
        turn_id: Option<&TurnId>,
        event_type: &str,
        payload: &str,
        now: Timestamp,
    ) -> Result<EventRecord, crate::OwnershipError>;
}

/// The production SQLite adapter: delegates to [`EventRepository`].
#[async_trait]
impl EventStore for Database {
    async fn append(
        &self,
        session_id: &SessionId,
        turn_id: Option<&TurnId>,
        event_type: &str,
        payload: &str,
        now: Timestamp,
    ) -> Result<EventRecord, StorageError> {
        EventRepository::new(self)
            .append(session_id, turn_id, event_type, payload, now)
            .await
    }

    async fn load(&self, session_id: &SessionId) -> Result<Vec<EventRecord>, StorageError> {
        EventRepository::new(self).load(session_id).await
    }

    async fn load_after(
        &self,
        session_id: &SessionId,
        after: i64,
    ) -> Result<Vec<EventRecord>, StorageError> {
        EventRepository::new(self)
            .load_after(session_id, after)
            .await
    }

    async fn load_last_by_type(
        &self,
        session_id: &SessionId,
        event_type: &str,
        turn_id: Option<&TurnId>,
    ) -> Result<Option<EventRecord>, StorageError> {
        EventRepository::new(self)
            .load_last_by_type(session_id, event_type, turn_id)
            .await
    }

    async fn load_window(
        &self,
        session_id: &SessionId,
        from_seq: i64,
        to_seq: i64,
    ) -> Result<Vec<EventRecord>, StorageError> {
        EventRepository::new(self)
            .load_window(session_id, from_seq, to_seq)
            .await
    }

    async fn latest_sequence(&self, session_id: &SessionId) -> Result<Option<i64>, StorageError> {
        EventRepository::new(self).latest_sequence(session_id).await
    }

    async fn count_by_type(
        &self,
        session_id: &SessionId,
    ) -> Result<Vec<(String, i64)>, StorageError> {
        EventRepository::new(self).count_by_type(session_id).await
    }

    async fn load_by_types(
        &self,
        session_id: &SessionId,
        types: &[&str],
    ) -> Result<Vec<EventRecord>, StorageError> {
        EventRepository::new(self)
            .load_by_types(session_id, types)
            .await
    }

    async fn append_owned(
        &self,
        token: &leveler_core::OwnershipToken,
        session_id: &SessionId,
        turn_id: Option<&TurnId>,
        event_type: &str,
        payload: &str,
        now: Timestamp,
    ) -> Result<EventRecord, crate::OwnershipError> {
        let id = leveler_core::EventId::generate().into_inner();
        // Structure-aware redaction (R007 F2): fail loud before the INSERT.
        let payload = redact_validated(event_type, payload, session_id)
            .map_err(crate::OwnershipError::Storage)?;
        // One guarded statement: sequence assignment AND the ownership check
        // happen inside the INSERT itself, so there is no window between
        // "token verified" and "row written".
        let inserted = sqlx::query(
            "INSERT INTO events \
             (id, session_id, turn_id, sequence, type, payload, created_at, schema_version) \
             SELECT ?1, ?2, ?3, \
                    (SELECT COALESCE(MAX(sequence), 0) + 1 FROM events WHERE session_id = ?2), \
                    ?4, ?5, ?6, ?7 \
             WHERE EXISTS (SELECT 1 FROM tasks WHERE session_id = ?2 \
                           AND id = ?8 AND owner_runtime_id = ?9 AND owner_epoch = ?10)",
        )
        .bind(&id)
        .bind(session_id.as_str())
        .bind(turn_id.map(|t| t.as_str().to_string()))
        .bind(event_type)
        .bind(&payload)
        .bind(now.to_rfc3339())
        .bind(EVENT_SCHEMA_VERSION)
        .bind(token.task_id.as_str())
        .bind(token.runtime_id.as_str())
        .bind(token.owner_epoch.get() as i64)
        .execute(self.pool())
        .await
        .map_err(StorageError::from)?;
        if inserted.rows_affected() != 1 {
            return Err(crate::ownership_store::sqlite_stale_error(self, token).await);
        }
        Ok(sqlx::query_as::<_, EventRecord>(
            "SELECT id, session_id, turn_id, sequence, type AS event_type, payload, created_at, \
             schema_version FROM events WHERE id = ?1",
        )
        .bind(&id)
        .fetch_one(self.pool())
        .await
        .map_err(StorageError::from)?)
    }
}

/// An in-memory [`EventStore`] for tests and ephemeral runs. Mirrors the SQLite
/// adapter's contract: gapless per-session sequences assigned in insertion
/// order.
#[derive(Default)]
pub struct MemoryEventStore {
    events: Mutex<Vec<EventRecord>>,
    /// Shared ownership authority for fenced appends. Unset = `append_owned`
    /// fails loudly (safe direction: a fenced write can never silently run
    /// unfenced).
    ownership: std::sync::OnceLock<std::sync::Arc<crate::MemoryOwnershipState>>,
}

impl MemoryEventStore {
    /// An empty store. Events live only as long as the value itself.
    pub fn new() -> Self {
        Self::default()
    }

    /// Couple this store to the shared ownership authority so `append_owned`
    /// can enforce the fence.
    pub fn with_ownership(self, state: std::sync::Arc<crate::MemoryOwnershipState>) -> Self {
        let _ = self.ownership.set(state);
        self
    }

    /// Insert a pre-built record verbatim (e.g. a migration/version fixture),
    /// bypassing sequence assignment. For tests and replaying external fixtures.
    pub fn seed(&self, record: EventRecord) {
        self.events.lock().unwrap().push(record);
    }

    /// Test-only: push a raw record BELOW the validating write boundary,
    /// simulating a legacy row persisted by the pre-R007-fix scrubber.
    /// Production code must never call this — new writes go through
    /// [`redact_validated`] and can no longer produce such rows.
    #[doc(hidden)]
    pub fn inject_legacy_row_for_tests(&self, record: EventRecord) {
        self.events.lock().unwrap().push(record);
    }

    /// Crate-internal synchronous append for the memory terminal store,
    /// whose commit body must run under the ownership lock. Redacts and
    /// validates the payload up front; a refused payload never reaches the log.
    pub(crate) fn append_record_for_terminal_validated(
        &self,
        session_id: &SessionId,
        turn_id: Option<&TurnId>,
        event_type: &str,
        payload: &str,
        now: Timestamp,
    ) -> Result<EventRecord, StorageError> {
        let payload = redact_validated(event_type, payload, session_id)?;
        Ok(self.append_record(session_id, turn_id, event_type, &payload, now))
    }

    /// The synchronous append shared by the fenced and unfenced paths. The
    /// payload MUST already be redacted+validated by the caller (all public
    /// entry points go through [`redact_validated`]).
    fn append_record(
        &self,
        session_id: &SessionId,
        turn_id: Option<&TurnId>,
        event_type: &str,
        payload: &str,
        now: Timestamp,
    ) -> EventRecord {
        let mut events = self.events.lock().unwrap();
        let sequence = events
            .iter()
            .filter(|e| e.session_id == session_id.as_str())
            .map(|e| e.sequence)
            .max()
            .unwrap_or(0)
            + 1;
        let record = EventRecord {
            id: leveler_core::EventId::generate().into_inner(),
            session_id: session_id.as_str().to_string(),
            turn_id: turn_id.map(|t| t.as_str().to_string()),
            sequence,
            event_type: event_type.to_string(),
            payload: payload.to_string(),
            created_at: now.to_rfc3339(),
            schema_version: EVENT_SCHEMA_VERSION,
        };
        events.push(record.clone());
        record
    }
}

#[async_trait]
impl EventStore for MemoryEventStore {
    async fn append(
        &self,
        session_id: &SessionId,
        turn_id: Option<&TurnId>,
        event_type: &str,
        payload: &str,
        now: Timestamp,
    ) -> Result<EventRecord, StorageError> {
        let payload = redact_validated(event_type, payload, session_id)?;
        Ok(self.append_record(session_id, turn_id, event_type, &payload, now))
    }

    async fn append_owned(
        &self,
        token: &leveler_core::OwnershipToken,
        session_id: &SessionId,
        turn_id: Option<&TurnId>,
        event_type: &str,
        payload: &str,
        now: Timestamp,
    ) -> Result<EventRecord, crate::OwnershipError> {
        let Some(ownership) = self.ownership.get() else {
            return Err(crate::OwnershipError::Storage(StorageError::InvalidData(
                "memory event store has no ownership authority configured".to_string(),
            )));
        };
        let payload = redact_validated(event_type, payload, session_id)
            .map_err(crate::OwnershipError::Storage)?;
        // Ownership lock held across the append — no interleaved CAS window.
        ownership.with_current(token, || {
            self.append_record(session_id, turn_id, event_type, &payload, now)
        })
    }

    async fn load(&self, session_id: &SessionId) -> Result<Vec<EventRecord>, StorageError> {
        self.load_after(session_id, 0).await
    }

    async fn load_after(
        &self,
        session_id: &SessionId,
        after: i64,
    ) -> Result<Vec<EventRecord>, StorageError> {
        let events = self.events.lock().unwrap();
        let mut rows: Vec<EventRecord> = events
            .iter()
            .filter(|e| e.session_id == session_id.as_str() && e.sequence > after)
            .cloned()
            .collect();
        rows.sort_by_key(|e| e.sequence);
        Ok(rows)
    }

    async fn load_last_by_type(
        &self,
        session_id: &SessionId,
        event_type: &str,
        turn_id: Option<&TurnId>,
    ) -> Result<Option<EventRecord>, StorageError> {
        let events = self.events.lock().unwrap();
        Ok(events
            .iter()
            .filter(|e| {
                e.session_id == session_id.as_str()
                    && e.event_type == event_type
                    && turn_id.is_none_or(|t| e.turn_id.as_deref() == Some(t.as_str()))
            })
            .max_by_key(|e| e.sequence)
            .cloned())
    }

    async fn load_window(
        &self,
        session_id: &SessionId,
        from_seq: i64,
        to_seq: i64,
    ) -> Result<Vec<EventRecord>, StorageError> {
        let events = self.events.lock().unwrap();
        let mut rows: Vec<EventRecord> = events
            .iter()
            .filter(|e| {
                e.session_id == session_id.as_str()
                    && e.sequence >= from_seq
                    && e.sequence <= to_seq
            })
            .cloned()
            .collect();
        rows.sort_by_key(|e| e.sequence);
        Ok(rows)
    }

    async fn latest_sequence(&self, session_id: &SessionId) -> Result<Option<i64>, StorageError> {
        Ok(self
            .events
            .lock()
            .unwrap()
            .iter()
            .filter(|e| e.session_id == session_id.as_str())
            .map(|e| e.sequence)
            .max())
    }

    async fn count_by_type(
        &self,
        session_id: &SessionId,
    ) -> Result<Vec<(String, i64)>, StorageError> {
        let events = self.events.lock().unwrap();
        let mut order: Vec<String> = Vec::new();
        let mut counts: std::collections::HashMap<String, i64> = std::collections::HashMap::new();
        for e in events
            .iter()
            .filter(|e| e.session_id == session_id.as_str())
        {
            let entry = counts.entry(e.event_type.clone()).or_insert_with(|| {
                order.push(e.event_type.clone());
                0
            });
            *entry += 1;
        }
        Ok(order
            .into_iter()
            .map(|t| {
                let n = counts[&t];
                (t, n)
            })
            .collect())
    }

    async fn load_by_types(
        &self,
        session_id: &SessionId,
        types: &[&str],
    ) -> Result<Vec<EventRecord>, StorageError> {
        if types.is_empty() {
            return Ok(Vec::new());
        }
        let events = self.events.lock().unwrap();
        let mut rows: Vec<EventRecord> = events
            .iter()
            .filter(|e| {
                e.session_id == session_id.as_str() && types.iter().any(|t| e.event_type == *t)
            })
            .cloned()
            .collect();
        rows.sort_by_key(|e| e.sequence);
        Ok(rows)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{SessionRecord, SessionRepository};

    /// One contract exercised against both implementations: append returns
    /// gapless per-session sequences and load reads them back in order.
    async fn assert_gapless_and_ordered(store: &dyn EventStore, session: &SessionId) {
        for i in 0..4 {
            let rec = store
                .append(
                    session,
                    None,
                    "e",
                    &format!("{{\"i\":{i}}}"),
                    leveler_core::now(),
                )
                .await
                .unwrap();
            assert_eq!(rec.sequence, i + 1);
        }
        let loaded = store.load(session).await.unwrap();
        assert_eq!(
            loaded.iter().map(|e| e.sequence).collect::<Vec<_>>(),
            vec![1, 2, 3, 4]
        );
        let newer = store.load_after(session, 2).await.unwrap();
        assert_eq!(
            newer.iter().map(|e| e.sequence).collect::<Vec<_>>(),
            vec![3, 4]
        );
    }

    /// Both implementations: newest row of the requested type wins, turn
    /// scoping filters, and a missing type is `None` (not an error).
    async fn assert_last_by_type(store: &dyn EventStore, session: &SessionId, turn_a: TurnId) {
        store
            .append(
                session,
                None,
                "plan_updated",
                r#"{"v":1}"#,
                leveler_core::now(),
            )
            .await
            .unwrap();
        store
            .append(
                session,
                Some(&turn_a),
                "plan_updated",
                r#"{"v":2}"#,
                leveler_core::now(),
            )
            .await
            .unwrap();
        store
            .append(session, None, "other", r#"{"v":3}"#, leveler_core::now())
            .await
            .unwrap();

        let latest = store
            .load_last_by_type(session, "plan_updated", None)
            .await
            .unwrap()
            .expect("latest plan row");
        assert_eq!(latest.payload, r#"{"v":2}"#, "newest of the type wins");

        let scoped = store
            .load_last_by_type(session, "plan_updated", Some(&turn_a))
            .await
            .unwrap()
            .expect("turn-scoped row");
        assert_eq!(scoped.payload, r#"{"v":2}"#);
        assert!(
            store
                .load_last_by_type(session, "missing_type", None)
                .await
                .unwrap()
                .is_none()
        );
    }

    #[tokio::test]
    async fn memory_store_honors_the_contract() {
        let store = MemoryEventStore::new();
        assert_gapless_and_ordered(&store, &SessionId::generate()).await;
    }

    #[tokio::test]
    async fn sqlite_store_honors_the_contract() {
        let db = Database::connect_in_memory().await.unwrap();
        let record = SessionRecord::new("/repo", "goal", "mock/m", leveler_core::now());
        SessionRepository::new(&db).create(&record).await.unwrap();
        assert_gapless_and_ordered(&db, &SessionId::new(record.id)).await;
    }

    #[tokio::test]
    async fn memory_store_last_by_type() {
        let store = MemoryEventStore::new();
        let session = SessionId::generate();
        assert_last_by_type(&store, &session, TurnId::new("turn-a".to_string())).await;
    }

    #[tokio::test]
    async fn sqlite_store_last_by_type() {
        let db = Database::connect_in_memory().await.unwrap();
        let record = SessionRecord::new("/repo", "goal", "mock/m", leveler_core::now());
        SessionRepository::new(&db).create(&record).await.unwrap();
        let session = SessionId::new(record.id);
        // events.turn_id has a foreign key: use a real persisted turn.
        let turn = crate::TurnRepository::new(&db)
            .start(&session, "node", None, leveler_core::now())
            .await
            .unwrap();
        assert_last_by_type(&db, &session, TurnId::new(turn.id)).await;
    }

    #[tokio::test]
    async fn memory_store_scopes_sequences_per_session() {
        let store = MemoryEventStore::new();
        let a = SessionId::generate();
        let b = SessionId::generate();
        store
            .append(&a, None, "x", "{}", leveler_core::now())
            .await
            .unwrap();
        let rec = store
            .append(&b, None, "x", "{}", leveler_core::now())
            .await
            .unwrap();
        assert_eq!(rec.sequence, 1, "sequences must not leak across sessions");
    }
}
