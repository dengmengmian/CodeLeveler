//! The `MessageStore` and `ModelRequestStore` ports: the transcript seams the
//! engine's turn runner depends on.
//!
//! `MessageStore` covers exactly what the engine needs — ordered, turn-stamped
//! append (the durable-transcript barrier behind `TurnSink`) and whole-session
//! load (`RawTranscript`). App-side conveniences on
//! [`crate::MessageRepository`] (truncation, checkpoint rewrite, counting, UI
//! projections) stay concrete.
//!
//! `ModelRequestStore` is separate on purpose: a model-request row is
//! telemetry about one upstream call, not part of the conversation transcript.

use std::sync::Mutex;

use async_trait::async_trait;

use leveler_core::{SessionId, Timestamp, TurnId};

use crate::{Database, MessageRepository, ModelRequestRecord, StorageError};

/// The engine-facing transcript persistence contract.
#[async_trait]
pub trait MessageStore: Send + Sync {
    /// Append `payloads` in order, stamped with `turn_id`. An error means the
    /// transcript is NOT durable — callers must propagate, never continue as
    /// if it were. Secrets are redacted on write.
    async fn append_in_turn(
        &self,
        session_id: &SessionId,
        turn_id: &TurnId,
        payloads: &[String],
        now: Timestamp,
    ) -> Result<(), StorageError>;

    /// The session's full transcript payloads, in append order.
    async fn load(&self, session_id: &SessionId) -> Result<Vec<String>, StorageError>;

    /// Fenced append: the ownership check and the inserts share one atomic
    /// persistence boundary. A stale runtime cannot extend the transcript.
    async fn append_in_turn_owned(
        &self,
        token: &leveler_core::OwnershipToken,
        session_id: &SessionId,
        turn_id: &TurnId,
        payloads: &[String],
        now: Timestamp,
    ) -> Result<(), crate::OwnershipError>;
}

/// The engine-facing model-request telemetry contract.
#[async_trait]
pub trait ModelRequestStore: Send + Sync {
    /// Record one upstream model call.
    async fn insert(&self, record: &ModelRequestRecord) -> Result<(), StorageError>;
}

/// The production SQLite adapters.
#[async_trait]
impl MessageStore for Database {
    async fn append_in_turn(
        &self,
        session_id: &SessionId,
        turn_id: &TurnId,
        payloads: &[String],
        now: Timestamp,
    ) -> Result<(), StorageError> {
        MessageRepository::new(self)
            .append_in_turn(session_id, turn_id, payloads, now)
            .await
    }

    async fn load(&self, session_id: &SessionId) -> Result<Vec<String>, StorageError> {
        MessageRepository::new(self).load(session_id).await
    }

    async fn append_in_turn_owned(
        &self,
        token: &leveler_core::OwnershipToken,
        session_id: &SessionId,
        turn_id: &TurnId,
        payloads: &[String],
        now: Timestamp,
    ) -> Result<(), crate::OwnershipError> {
        MessageRepository::new(self)
            .append_in_turn_owned(token, session_id, turn_id, payloads, now)
            .await
    }
}

#[async_trait]
impl ModelRequestStore for Database {
    async fn insert(&self, record: &ModelRequestRecord) -> Result<(), StorageError> {
        crate::ModelRequestRepository::new(self)
            .insert(record)
            .await
    }
}

/// An in-memory [`MessageStore`] honoring the same contract: ordered append,
/// per-session isolation, and write-time secret redaction.
#[derive(Default)]
pub struct MemoryMessageStore {
    /// `(session_id, payload)` in append order.
    rows: Mutex<Vec<(String, String)>>,
    ownership: std::sync::OnceLock<std::sync::Arc<crate::MemoryOwnershipState>>,
}

impl MemoryMessageStore {
    /// An empty store.
    pub fn new() -> Self {
        Self::default()
    }

    /// Couple to the shared ownership authority for fenced appends.
    pub fn with_ownership(self, state: std::sync::Arc<crate::MemoryOwnershipState>) -> Self {
        let _ = self.ownership.set(state);
        self
    }

    fn append_records(&self, session_id: &SessionId, payloads: &[String]) {
        let mut rows = self.rows.lock().unwrap();
        for payload in payloads {
            rows.push((
                session_id.as_str().to_string(),
                leveler_core::redact_secrets(payload),
            ));
        }
    }
}

#[async_trait]
impl MessageStore for MemoryMessageStore {
    async fn append_in_turn(
        &self,
        session_id: &SessionId,
        _turn_id: &TurnId,
        payloads: &[String],
        _now: Timestamp,
    ) -> Result<(), StorageError> {
        self.append_records(session_id, payloads);
        Ok(())
    }

    async fn append_in_turn_owned(
        &self,
        token: &leveler_core::OwnershipToken,
        session_id: &SessionId,
        _turn_id: &TurnId,
        payloads: &[String],
        _now: Timestamp,
    ) -> Result<(), crate::OwnershipError> {
        let Some(ownership) = self.ownership.get() else {
            return Err(crate::OwnershipError::Storage(StorageError::InvalidData(
                "memory message store has no ownership authority configured".to_string(),
            )));
        };
        ownership.with_current(token, || self.append_records(session_id, payloads))
    }

    async fn load(&self, session_id: &SessionId) -> Result<Vec<String>, StorageError> {
        Ok(self
            .rows
            .lock()
            .unwrap()
            .iter()
            .filter(|(s, _)| s == session_id.as_str())
            .map(|(_, p)| p.clone())
            .collect())
    }
}

/// An in-memory [`ModelRequestStore`].
#[derive(Default)]
pub struct MemoryModelRequestStore {
    rows: Mutex<Vec<ModelRequestRecord>>,
}

impl MemoryModelRequestStore {
    /// An empty store.
    pub fn new() -> Self {
        Self::default()
    }

    /// Test hook: how many requests were recorded.
    pub fn len(&self) -> usize {
        self.rows.lock().unwrap().len()
    }

    /// Test hook: whether nothing was recorded.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

#[async_trait]
impl ModelRequestStore for MemoryModelRequestStore {
    async fn insert(&self, record: &ModelRequestRecord) -> Result<(), StorageError> {
        self.rows.lock().unwrap().push(record.clone());
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{SessionRecord, SessionRepository, TurnRepository};

    /// One contract against both implementations: append order is load order,
    /// sessions are isolated, and secrets never come back out.
    async fn assert_message_store_contract(
        store: &dyn MessageStore,
        session_a: &SessionId,
        turn_a: &TurnId,
        session_b: &SessionId,
        turn_b: &TurnId,
    ) {
        assert!(store.load(session_a).await.unwrap().is_empty());
        store
            .append_in_turn(
                session_a,
                turn_a,
                &["one".to_string(), "two".to_string()],
                leveler_core::now(),
            )
            .await
            .unwrap();
        store
            .append_in_turn(
                session_b,
                turn_b,
                &[r#"{"api_key":"super-secret-value"}"#.to_string()],
                leveler_core::now(),
            )
            .await
            .unwrap();
        store
            .append_in_turn(
                session_a,
                turn_a,
                &["three".to_string()],
                leveler_core::now(),
            )
            .await
            .unwrap();

        assert_eq!(
            store.load(session_a).await.unwrap(),
            vec!["one", "two", "three"],
            "append order must be load order"
        );
        let b = store.load(session_b).await.unwrap();
        assert_eq!(b.len(), 1, "sessions must be isolated");
        assert!(
            !b[0].contains("super-secret-value"),
            "secrets must be redacted on write: {b:?}"
        );
    }

    #[tokio::test]
    async fn sqlite_store_honors_the_contract() {
        let db = Database::connect_in_memory().await.unwrap();
        let a = SessionRecord::new("/repo", "a", "mock/m", leveler_core::now());
        let b = SessionRecord::new("/repo", "b", "mock/m", leveler_core::now());
        SessionRepository::new(&db).create(&a).await.unwrap();
        SessionRepository::new(&db).create(&b).await.unwrap();
        let session_a = SessionId::new(a.id);
        let session_b = SessionId::new(b.id);
        let turn_a = TurnRepository::new(&db)
            .start(&session_a, "user", None, leveler_core::now())
            .await
            .unwrap();
        let turn_b = TurnRepository::new(&db)
            .start(&session_b, "user", None, leveler_core::now())
            .await
            .unwrap();
        assert_message_store_contract(
            &db,
            &session_a,
            &TurnId::new(turn_a.id),
            &session_b,
            &TurnId::new(turn_b.id),
        )
        .await;
    }

    #[tokio::test]
    async fn memory_store_honors_the_contract() {
        let store = MemoryMessageStore::new();
        assert_message_store_contract(
            &store,
            &SessionId::generate(),
            &TurnId::new("t-a"),
            &SessionId::generate(),
            &TurnId::new("t-b"),
        )
        .await;
    }
}
