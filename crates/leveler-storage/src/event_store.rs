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
}

/// An in-memory [`EventStore`] for tests and ephemeral runs. Mirrors the SQLite
/// adapter's contract: gapless per-session sequences assigned in insertion
/// order.
#[derive(Default)]
pub struct MemoryEventStore {
    events: Mutex<Vec<EventRecord>>,
}

impl MemoryEventStore {
    pub fn new() -> Self {
        Self::default()
    }

    /// Insert a pre-built record verbatim (e.g. a migration/version fixture),
    /// bypassing sequence assignment. For tests and replaying external fixtures.
    pub fn seed(&self, record: EventRecord) {
        self.events.lock().unwrap().push(record);
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
        Ok(record)
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
        assert_last_of_type(&store, &SessionId::generate()).await;
    }

    #[tokio::test]
    async fn sqlite_store_honors_the_contract() {
        let db = Database::connect_in_memory().await.unwrap();
        let record = SessionRecord::new("/repo", "goal", "mock/m", leveler_core::now());
        SessionRepository::new(&db).create(&record).await.unwrap();
        let session = SessionId::new(record.id);
        assert_gapless_and_ordered(&db, &session).await;
        assert_last_of_type(&db, &session).await;
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
