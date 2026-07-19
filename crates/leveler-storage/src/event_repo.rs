//! Append-only event log: every engine event is persisted with a
//! gapless per-session sequence BEFORE it is delivered to observers, so a
//! crashed run can be reconstructed exactly.

use serde::{Deserialize, Serialize};

use leveler_core::{SessionId, Timestamp, TurnId};

use crate::database::{Database, StorageError};

/// The payload format version written for new events. Replay rejects any
/// persisted version greater than this (a newer writer's event an older reader
/// cannot understand) instead of guessing.
pub const EVENT_SCHEMA_VERSION: i64 = 1;

/// A persisted event row.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, sqlx::FromRow)]
pub struct EventRecord {
    pub id: String,
    pub session_id: String,
    pub turn_id: Option<String>,
    pub sequence: i64,
    /// The event's type tag (queryable without parsing the payload).
    pub event_type: String,
    /// The event's JSON payload.
    pub payload: String,
    pub created_at: String,
    /// The payload format version (see [`EVENT_SCHEMA_VERSION`]).
    pub schema_version: i64,
}

/// Read/write access to the `events` table.
pub struct EventRepository<'a> {
    db: &'a Database,
}

impl<'a> EventRepository<'a> {
    pub fn new(db: &'a Database) -> Self {
        Self { db }
    }

    /// Append one event, assigning the next per-session sequence atomically.
    /// Returns the persisted record (with its sequence).
    pub async fn append(
        &self,
        session_id: &SessionId,
        turn_id: Option<&TurnId>,
        event_type: &str,
        payload: &str,
        now: Timestamp,
    ) -> Result<EventRecord, StorageError> {
        let id = leveler_core::EventId::generate().into_inner();
        let payload = leveler_core::redact_secrets(payload);
        // The sequence is assigned inside the INSERT so concurrent appends on
        // one connection pool cannot race a read-then-write; the UNIQUE index on
        // (session_id, sequence) is the backstop that turns any residual race
        // into a loud failure instead of silent reordering.
        sqlx::query(
            "INSERT INTO events \
             (id, session_id, turn_id, sequence, type, payload, created_at, schema_version) \
             SELECT ?1, ?2, ?3, COALESCE(MAX(sequence), 0) + 1, ?4, ?5, ?6, ?7 \
             FROM events WHERE session_id = ?2",
        )
        .bind(&id)
        .bind(session_id.as_str())
        .bind(turn_id.map(|t| t.as_str().to_string()))
        .bind(event_type)
        .bind(&payload)
        .bind(now.to_rfc3339())
        .bind(EVENT_SCHEMA_VERSION)
        .execute(self.db.pool())
        .await?;
        let row = sqlx::query_as::<_, EventRecord>(
            "SELECT id, session_id, turn_id, sequence, type AS event_type, payload, created_at, \
             schema_version FROM events WHERE id = ?1",
        )
        .bind(&id)
        .fetch_one(self.db.pool())
        .await?;
        Ok(row)
    }

    /// All events of a session in sequence order.
    pub async fn load(&self, session_id: &SessionId) -> Result<Vec<EventRecord>, StorageError> {
        self.load_after(session_id, 0).await
    }

    /// The highest sequence persisted for a session — the resync anchor a
    /// snapshot advertises. `None` when the session has no events yet.
    pub async fn latest_sequence(
        &self,
        session_id: &SessionId,
    ) -> Result<Option<i64>, StorageError> {
        let row: Option<(Option<i64>,)> =
            sqlx::query_as("SELECT MAX(sequence) FROM events WHERE session_id = ?1")
                .bind(session_id.as_str())
                .fetch_optional(self.db.pool())
                .await?;
        Ok(row.and_then(|(seq,)| seq))
    }

    /// Events with `sequence > after`, in sequence order.
    pub async fn load_after(
        &self,
        session_id: &SessionId,
        after: i64,
    ) -> Result<Vec<EventRecord>, StorageError> {
        let rows = sqlx::query_as::<_, EventRecord>(
            "SELECT id, session_id, turn_id, sequence, type AS event_type, payload, created_at, \
             schema_version FROM events WHERE session_id = ?1 AND sequence > ?2 ORDER BY sequence",
        )
        .bind(session_id.as_str())
        .bind(after)
        .fetch_all(self.db.pool())
        .await?;
        Ok(rows)
    }

    /// The newest event of `event_type`, optionally scoped to one turn — an
    /// indexed single-row lookup so "latest plan/ledger/snapshot" seeding never
    /// scans the whole session log.
    pub async fn load_last_by_type(
        &self,
        session_id: &SessionId,
        event_type: &str,
        turn_id: Option<&leveler_core::TurnId>,
    ) -> Result<Option<EventRecord>, StorageError> {
        let row = sqlx::query_as::<_, EventRecord>(
            "SELECT id, session_id, turn_id, sequence, type AS event_type, payload, created_at, \
             schema_version FROM events \
             WHERE session_id = ?1 AND type = ?2 AND (?3 IS NULL OR turn_id = ?3) \
             ORDER BY sequence DESC LIMIT 1",
        )
        .bind(session_id.as_str())
        .bind(event_type)
        .bind(turn_id.map(|t| t.as_str()))
        .fetch_optional(self.db.pool())
        .await?;
        Ok(row)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session_repo::{SessionRecord, SessionRepository};
    use crate::turn_repo::TurnRepository;

    async fn db_with_session() -> (Database, SessionId) {
        let db = Database::connect_in_memory().await.unwrap();
        let record = SessionRecord::new("/repo", "goal", "mock/m", leveler_core::now());
        SessionRepository::new(&db).create(&record).await.unwrap();
        let id = SessionId::new(record.id);
        (db, id)
    }

    #[tokio::test]
    async fn append_assigns_gapless_sequences() {
        let (db, session) = db_with_session().await;
        let repo = EventRepository::new(&db);

        for i in 0..5 {
            let rec = repo
                .append(
                    &session,
                    None,
                    "task_started",
                    &format!(r#"{{"i":{i}}}"#),
                    leveler_core::now(),
                )
                .await
                .unwrap();
            assert_eq!(rec.sequence, i + 1, "sequences are 1-based and gapless");
        }

        let loaded = repo.load(&session).await.unwrap();
        assert_eq!(
            loaded.iter().map(|e| e.sequence).collect::<Vec<_>>(),
            vec![1, 2, 3, 4, 5]
        );
        assert_eq!(loaded[0].event_type, "task_started");
        assert_eq!(loaded[2].payload, r#"{"i":2}"#);
    }

    #[tokio::test]
    async fn sequences_are_scoped_per_session() {
        let (db, a) = db_with_session().await;
        let record = SessionRecord::new("/repo", "other", "mock/m", leveler_core::now());
        SessionRepository::new(&db).create(&record).await.unwrap();
        let b = SessionId::new(record.id);

        let repo = EventRepository::new(&db);
        repo.append(&a, None, "x", "{}", leveler_core::now())
            .await
            .unwrap();
        let rec = repo
            .append(&b, None, "x", "{}", leveler_core::now())
            .await
            .unwrap();
        assert_eq!(rec.sequence, 1, "sequences must not leak across sessions");
    }

    #[tokio::test]
    async fn new_events_carry_the_current_schema_version() {
        let (db, session) = db_with_session().await;
        let rec = EventRepository::new(&db)
            .append(&session, None, "x", "{}", leveler_core::now())
            .await
            .unwrap();
        assert_eq!(rec.schema_version, EVENT_SCHEMA_VERSION);
    }

    #[tokio::test]
    async fn duplicate_sequence_is_rejected_by_the_unique_index() {
        let (db, session) = db_with_session().await;
        EventRepository::new(&db)
            .append(&session, None, "x", "{}", leveler_core::now())
            .await
            .unwrap();
        // A manual row colliding on (session_id, sequence=1) must be refused —
        // the log can never silently reorder or overwrite.
        let dup = sqlx::query(
            "INSERT INTO events \
             (id, session_id, turn_id, sequence, type, payload, created_at, schema_version) \
             VALUES (?1, ?2, NULL, 1, 'x', '{}', ?3, 1)",
        )
        .bind(leveler_core::EventId::generate().into_inner())
        .bind(session.as_str())
        .bind(leveler_core::now().to_rfc3339())
        .execute(db.pool())
        .await;
        assert!(
            dup.is_err(),
            "duplicate sequence must violate the unique index"
        );
    }

    #[tokio::test]
    async fn load_after_returns_only_newer_events() {
        let (db, session) = db_with_session().await;
        let repo = EventRepository::new(&db);
        for _ in 0..3 {
            repo.append(&session, None, "x", "{}", leveler_core::now())
                .await
                .unwrap();
        }
        let newer = repo.load_after(&session, 1).await.unwrap();
        assert_eq!(
            newer.iter().map(|e| e.sequence).collect::<Vec<_>>(),
            vec![2, 3]
        );
    }

    #[tokio::test]
    async fn events_can_belong_to_a_turn() {
        let (db, session) = db_with_session().await;
        let turn = TurnRepository::new(&db)
            .start(&session, "user", None, leveler_core::now())
            .await
            .unwrap();
        let repo = EventRepository::new(&db);
        let rec = repo
            .append(
                &session,
                Some(&leveler_core::TurnId::new(turn.id.clone())),
                "turn_started",
                "{}",
                leveler_core::now(),
            )
            .await
            .unwrap();
        assert_eq!(rec.turn_id.as_deref(), Some(turn.id.as_str()));
    }

    /// Legacy-shaped rows (pre-0003 columns omitted) must read the migration
    /// defaults — this is what an existing database sees after upgrading.
    #[tokio::test]
    async fn legacy_rows_read_migration_defaults() {
        let (db, session) = db_with_session().await;
        // A pre-0003 sessions row never wrote mode/sandbox/kind/outcome.
        let (mode, sandbox, kind, outcome): (String, i64, String, Option<String>) =
            sqlx::query_as("SELECT mode, sandbox, kind, outcome FROM sessions WHERE id = ?1")
                .bind(session.as_str())
                .fetch_one(db.pool())
                .await
                .unwrap();
        // 0003 default was workspace_write; 0012/0013 remap to assisted profiles.
        assert_eq!(mode, "assisted");
        assert_eq!(sandbox, 0);
        assert_eq!(kind, "direct");
        assert_eq!(outcome, None);
    }

    #[tokio::test]
    async fn append_redacts_json_secrets() {
        let (db, session) = db_with_session().await;
        let record = EventRepository::new(&db)
            .append(
                &session,
                None,
                "diagnostic",
                r#"{"Authorization":"Bearer event-secret-value"}"#,
                leveler_core::now(),
            )
            .await
            .unwrap();
        assert!(!record.payload.contains("event-secret-value"), "{record:?}");
        assert!(record.payload.contains("[REDACTED]"), "{record:?}");
    }
}
