//! The `GoalCheckpointStore` port: durable semantic checkpoints (P3).
//!
//! Same shape as [`crate::GoalStore`]: a narrow trait, storage owns the
//! SQLite adapter over `goal_checkpoints` (migration 0019), and
//! [`MemoryGoalCheckpointStore`] honors the identical contract.
//!
//! **This port records a derived projection, never truth.** The event log
//! stays canonical; a checkpoint row says "committed events of `session_id`
//! with `sequence <= event_cursor` are represented by this payload". The
//! caller owns capturing the cursor at a durable barrier — the store only
//! promises it will not invent, duplicate, or silently trust rows:
//!
//! - Idempotency is schema-enforced: `(goal_id, reason, event_cursor)` is
//!   unique, and [`GoalCheckpointStore::create`] returns the existing row on
//!   a repeat instead of failing or duplicating.
//! - Reads fail closed: a payload we cannot interpret (newer schema_version,
//!   unknown reason, malformed JSON) is never returned as a checkpoint.
//!   [`GoalCheckpointStore::latest_for_goal`] skips it and falls back to the
//!   previous valid one; [`GoalCheckpointStore::get`] surfaces the error.

use std::sync::Mutex;

use async_trait::async_trait;

use leveler_core::{GoalCheckpointId, GoalId, SessionId, Timestamp};
use leveler_lifecycle::{CheckpointReason, GOAL_CHECKPOINT_SCHEMA_VERSION, GoalCheckpoint};

use crate::{Database, StorageError};

/// One durable checkpoint.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GoalCheckpointRecord {
    /// Stable identity; what a TUI Recap item points back at.
    pub id: GoalCheckpointId,
    /// The goal this checkpoint projects.
    pub goal_id: GoalId,
    /// The session whose event log `event_cursor` indexes. A cursor is
    /// meaningless without its session.
    pub session_id: SessionId,
    /// Why it was cut. Part of the dedupe identity.
    pub reason: CheckpointReason,
    /// Inclusive, committed-only boundary: events `<= event_cursor` of
    /// `session_id` are represented. The recent delta starts at `+1`.
    pub event_cursor: i64,
    /// Payload format version this row was written with.
    pub schema_version: i64,
    /// The projected facts and semantic summary.
    pub payload: GoalCheckpoint,
    /// When it was cut.
    pub created_at: Timestamp,
}

/// What a writer supplies; id, version, and timestamp are the store's job.
#[derive(Debug, Clone)]
pub struct NewGoalCheckpoint {
    /// The goal this checkpoint projects.
    pub goal_id: GoalId,
    /// The session whose event log `event_cursor` indexes.
    pub session_id: SessionId,
    /// Why it was cut; part of the dedupe identity.
    pub reason: CheckpointReason,
    /// Inclusive committed boundary the caller captured at a durable barrier.
    pub event_cursor: i64,
    /// The projected facts and semantic summary.
    pub payload: GoalCheckpoint,
}

/// Durable access to goal checkpoints.
#[async_trait]
pub trait GoalCheckpointStore: Send + Sync {
    /// Persist a checkpoint, or return the existing one when the same
    /// semantic boundary `(goal_id, reason, event_cursor)` was already
    /// recorded. Repeats and races collapse to one logical checkpoint.
    async fn create(
        &self,
        new: NewGoalCheckpoint,
        now: Timestamp,
    ) -> Result<GoalCheckpointRecord, StorageError>;

    /// One checkpoint by id. Strict: an uninterpretable row is an error,
    /// never a silently trusted checkpoint.
    async fn get(
        &self,
        id: &GoalCheckpointId,
    ) -> Result<Option<GoalCheckpointRecord>, StorageError>;

    /// The newest VALID checkpoint for a goal, by how much of the log it
    /// represents (`event_cursor`, then recency). Rows this reader cannot
    /// interpret are skipped — the previous valid checkpoint wins, and
    /// `None` means the caller falls back to the full-history path.
    async fn latest_for_goal(
        &self,
        goal_id: &GoalId,
    ) -> Result<Option<GoalCheckpointRecord>, StorageError>;

    /// Every valid checkpoint for a goal, newest boundary first.
    async fn for_goal(&self, goal_id: &GoalId) -> Result<Vec<GoalCheckpointRecord>, StorageError>;
}

type Row = (
    String, // id
    String, // goal_id
    String, // session_id
    String, // reason
    i64,    // event_cursor
    i64,    // schema_version
    String, // payload
    String, // created_at
);

const COLUMNS: &str =
    "id, goal_id, session_id, reason, event_cursor, schema_version, payload, created_at";

/// The production SQLite adapter over `goal_checkpoints` (migration 0019).
#[async_trait]
impl GoalCheckpointStore for Database {
    async fn create(
        &self,
        new: NewGoalCheckpoint,
        now: Timestamp,
    ) -> Result<GoalCheckpointRecord, StorageError> {
        let id = GoalCheckpointId::new(leveler_core::new_uuid_string());
        let payload = serde_json::to_string(&new.payload)
            .map_err(|e| StorageError::InvalidData(format!("unencodable checkpoint: {e}")))?;
        let payload = crate::redact_json_payload_for_session(
            "goal checkpoint",
            &payload,
            Some(new.session_id.as_str()),
        )?;
        // ON CONFLICT DO NOTHING + read-back: the unique index owns dedupe,
        // so a concurrent duplicate is resolved by SQLite, not by us.
        sqlx::query(
            "INSERT INTO goal_checkpoints \
             (id, goal_id, session_id, reason, event_cursor, schema_version, payload, created_at) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8) \
             ON CONFLICT(goal_id, reason, event_cursor) DO NOTHING",
        )
        .bind(id.as_str())
        .bind(new.goal_id.as_str())
        .bind(new.session_id.as_str())
        .bind(new.reason.as_str())
        .bind(new.event_cursor)
        .bind(GOAL_CHECKPOINT_SCHEMA_VERSION)
        .bind(&payload)
        .bind(now.to_rfc3339())
        .execute(self.pool())
        .await?;
        let row: Row = sqlx::query_as(&format!(
            "SELECT {COLUMNS} FROM goal_checkpoints \
             WHERE goal_id = ?1 AND reason = ?2 AND event_cursor = ?3",
        ))
        .bind(new.goal_id.as_str())
        .bind(new.reason.as_str())
        .bind(new.event_cursor)
        .fetch_one(self.pool())
        .await?;
        record_from_row(row)
    }

    async fn get(
        &self,
        id: &GoalCheckpointId,
    ) -> Result<Option<GoalCheckpointRecord>, StorageError> {
        let row: Option<Row> = sqlx::query_as(&format!(
            "SELECT {COLUMNS} FROM goal_checkpoints WHERE id = ?1",
        ))
        .bind(id.as_str())
        .fetch_optional(self.pool())
        .await?;
        row.map(record_from_row).transpose()
    }

    async fn latest_for_goal(
        &self,
        goal_id: &GoalId,
    ) -> Result<Option<GoalCheckpointRecord>, StorageError> {
        Ok(self.valid_for_goal(goal_id).await?.into_iter().next())
    }

    async fn for_goal(&self, goal_id: &GoalId) -> Result<Vec<GoalCheckpointRecord>, StorageError> {
        self.valid_for_goal(goal_id).await
    }
}

impl Database {
    async fn valid_for_goal(
        &self,
        goal_id: &GoalId,
    ) -> Result<Vec<GoalCheckpointRecord>, StorageError> {
        let rows: Vec<Row> = sqlx::query_as(&format!(
            "SELECT {COLUMNS} FROM goal_checkpoints WHERE goal_id = ?1 \
             ORDER BY event_cursor DESC, created_at DESC, id DESC",
        ))
        .bind(goal_id.as_str())
        .fetch_all(self.pool())
        .await?;
        let mut out = Vec::with_capacity(rows.len());
        for row in rows {
            let id = row.0.clone();
            match record_from_row(row) {
                Ok(record) => out.push(record),
                // Fail closed per row: a checkpoint we cannot interpret is
                // never returned, and the previous valid one still is.
                Err(error) => {
                    tracing::warn!(%error, checkpoint = %id, "skipping uninterpretable goal checkpoint");
                }
            }
        }
        Ok(out)
    }
}

fn record_from_row(row: Row) -> Result<GoalCheckpointRecord, StorageError> {
    let (id, goal_id, session_id, reason, event_cursor, schema_version, payload, created_at) = row;
    if schema_version > GOAL_CHECKPOINT_SCHEMA_VERSION {
        return Err(StorageError::InvalidData(format!(
            "goal checkpoint `{id}` has schema_version {schema_version}, newer than supported \
             {GOAL_CHECKPOINT_SCHEMA_VERSION}; refusing to guess"
        )));
    }
    Ok(GoalCheckpointRecord {
        id: GoalCheckpointId::new(id.clone()),
        goal_id: GoalId::new(goal_id),
        session_id: SessionId::new(session_id),
        reason: CheckpointReason::parse(&reason).ok_or_else(|| {
            StorageError::InvalidData(format!(
                "goal checkpoint `{id}` has unknown reason `{reason}`"
            ))
        })?,
        event_cursor,
        schema_version,
        payload: serde_json::from_str(&payload).map_err(|e| {
            StorageError::InvalidData(format!("goal checkpoint `{id}` payload is malformed: {e}"))
        })?,
        created_at: parse_ts(&created_at)?,
    })
}

fn parse_ts(s: &str) -> Result<Timestamp, StorageError> {
    chrono::DateTime::parse_from_rfc3339(s)
        .map(|t| t.with_timezone(&chrono::Utc))
        .map_err(|e| StorageError::InvalidData(format!("bad timestamp `{s}`: {e}")))
}

/// An in-memory [`GoalCheckpointStore`] honoring the same contract.
#[derive(Default)]
pub struct MemoryGoalCheckpointStore {
    rows: Mutex<Vec<GoalCheckpointRecord>>,
}

impl MemoryGoalCheckpointStore {
    /// An empty store.
    pub fn new() -> Self {
        Self::default()
    }
}

#[async_trait]
impl GoalCheckpointStore for MemoryGoalCheckpointStore {
    async fn create(
        &self,
        new: NewGoalCheckpoint,
        now: Timestamp,
    ) -> Result<GoalCheckpointRecord, StorageError> {
        let mut rows = self.rows.lock().unwrap();
        if let Some(existing) = rows.iter().find(|r| {
            r.goal_id == new.goal_id && r.reason == new.reason && r.event_cursor == new.event_cursor
        }) {
            return Ok(existing.clone());
        }
        let record = GoalCheckpointRecord {
            id: GoalCheckpointId::new(leveler_core::new_uuid_string()),
            goal_id: new.goal_id,
            session_id: new.session_id,
            reason: new.reason,
            event_cursor: new.event_cursor,
            schema_version: GOAL_CHECKPOINT_SCHEMA_VERSION,
            payload: new.payload,
            created_at: now,
        };
        rows.push(record.clone());
        Ok(record)
    }

    async fn get(
        &self,
        id: &GoalCheckpointId,
    ) -> Result<Option<GoalCheckpointRecord>, StorageError> {
        Ok(self
            .rows
            .lock()
            .unwrap()
            .iter()
            .find(|r| &r.id == id)
            .cloned())
    }

    async fn latest_for_goal(
        &self,
        goal_id: &GoalId,
    ) -> Result<Option<GoalCheckpointRecord>, StorageError> {
        Ok(self.for_goal(goal_id).await?.into_iter().next())
    }

    async fn for_goal(&self, goal_id: &GoalId) -> Result<Vec<GoalCheckpointRecord>, StorageError> {
        let mut out: Vec<GoalCheckpointRecord> = self
            .rows
            .lock()
            .unwrap()
            .iter()
            .filter(|r| &r.goal_id == goal_id)
            .cloned()
            .collect();
        out.sort_by(|a, b| {
            b.event_cursor
                .cmp(&a.event_cursor)
                .then(b.created_at.cmp(&a.created_at))
                .then(b.id.as_str().cmp(a.id.as_str()))
        });
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;
    use crate::{GoalStore, SessionRecord, SessionRepository, TaskStore};

    fn payload(objective: &str) -> GoalCheckpoint {
        GoalCheckpoint {
            objective: objective.to_string(),
            transcript_ordinal: Some(7),
            next_action: Some("keep going".to_string()),
            ..Default::default()
        }
    }

    fn new_checkpoint(
        goal: &GoalId,
        session: &SessionId,
        reason: CheckpointReason,
        cursor: i64,
    ) -> NewGoalCheckpoint {
        NewGoalCheckpoint {
            goal_id: goal.clone(),
            session_id: session.clone(),
            reason,
            event_cursor: cursor,
            payload: payload("the objective"),
        }
    }

    /// One contract, both implementations.
    async fn assert_checkpoint_store_contract(
        store: &dyn GoalCheckpointStore,
        goal: &GoalId,
        other_goal: &GoalId,
        session: &SessionId,
    ) {
        assert!(store.latest_for_goal(goal).await.unwrap().is_none());

        let first = store
            .create(
                new_checkpoint(goal, session, CheckpointReason::Manual, 10),
                leveler_core::now(),
            )
            .await
            .unwrap();
        assert_eq!(first.event_cursor, 10);
        assert_eq!(first.reason, CheckpointReason::Manual);
        assert_eq!(first.schema_version, GOAL_CHECKPOINT_SCHEMA_VERSION);
        assert_eq!(first.payload.objective, "the objective");
        assert_eq!(first.payload.transcript_ordinal, Some(7));

        // Read-back by id returns the same record.
        assert_eq!(store.get(&first.id).await.unwrap().as_ref(), Some(&first));
        assert_eq!(
            store.get(&GoalCheckpointId::new("missing")).await.unwrap(),
            None
        );

        // Idempotency: the same semantic boundary is ONE logical checkpoint.
        let repeat = store
            .create(
                new_checkpoint(goal, session, CheckpointReason::Manual, 10),
                leveler_core::now(),
            )
            .await
            .unwrap();
        assert_eq!(
            repeat.id, first.id,
            "a repeat must not mint a new checkpoint"
        );
        assert_eq!(store.for_goal(goal).await.unwrap().len(), 1);

        // A different cursor is a genuinely new checkpoint, and latest is
        // decided by the boundary, not by insertion order.
        let later = store
            .create(
                new_checkpoint(goal, session, CheckpointReason::Milestone, 25),
                leveler_core::now(),
            )
            .await
            .unwrap();
        assert_ne!(later.id, first.id);
        let latest = store.latest_for_goal(goal).await.unwrap().unwrap();
        assert_eq!(latest.id, later.id);
        let all = store.for_goal(goal).await.unwrap();
        assert_eq!(
            all.iter().map(|r| r.event_cursor).collect::<Vec<_>>(),
            vec![25, 10],
            "newest boundary first"
        );

        // Goals are isolated from each other.
        assert!(store.latest_for_goal(other_goal).await.unwrap().is_none());
        store
            .create(
                new_checkpoint(other_goal, session, CheckpointReason::Manual, 99),
                leveler_core::now(),
            )
            .await
            .unwrap();
        assert_eq!(store.for_goal(goal).await.unwrap().len(), 2);
        assert_eq!(
            store
                .latest_for_goal(goal)
                .await
                .unwrap()
                .unwrap()
                .event_cursor,
            25,
            "another goal's checkpoint must not become this goal's latest"
        );
    }

    async fn seeded_goals(db: &Database) -> (GoalId, GoalId, SessionId) {
        let record = SessionRecord::new("/repo", "goal", "mock/m", leveler_core::now());
        SessionRepository::new(db).create(&record).await.unwrap();
        let session = SessionId::new(record.id);
        let task = db
            .ensure_for_session(&session, leveler_core::now())
            .await
            .unwrap();
        let goal = db.open(&task, "first", leveler_core::now()).await.unwrap();
        let other = db.open(&task, "second", leveler_core::now()).await.unwrap();
        (goal, other, session)
    }

    #[tokio::test]
    async fn memory_store_honors_the_contract() {
        let store = MemoryGoalCheckpointStore::new();
        assert_checkpoint_store_contract(
            &store,
            &GoalId::new("g1"),
            &GoalId::new("g2"),
            &SessionId::new("s1"),
        )
        .await;
    }

    #[tokio::test]
    async fn sqlite_store_honors_the_contract() {
        let db = Database::connect_in_memory().await.unwrap();
        let (goal, other, session) = seeded_goals(&db).await;
        assert_checkpoint_store_contract(&db, &goal, &other, &session).await;
    }

    /// Concurrency: racing writers at the same boundary converge on one
    /// logical checkpoint — SQLite's unique index is the arbiter.
    #[tokio::test]
    async fn concurrent_duplicates_collapse_to_one() {
        let db = Database::connect_in_memory().await.unwrap();
        let (goal, _, session) = seeded_goals(&db).await;
        let db = Arc::new(db);
        let mut handles = Vec::new();
        for _ in 0..8 {
            let db = db.clone();
            let goal = goal.clone();
            let session = session.clone();
            handles.push(tokio::spawn(async move {
                db.create(
                    new_checkpoint(&goal, &session, CheckpointReason::Interrupted, 5),
                    leveler_core::now(),
                )
                .await
                .unwrap()
            }));
        }
        let mut ids = std::collections::HashSet::new();
        for h in handles {
            ids.insert(h.await.unwrap().id);
        }
        assert_eq!(ids.len(), 1, "all racers must observe the same checkpoint");
        assert_eq!(db.for_goal(&goal).await.unwrap().len(), 1);
    }

    /// The whole point of the table: a checkpoint survives the process.
    #[tokio::test]
    async fn a_checkpoint_is_readable_after_reconnecting() {
        let dir = std::env::temp_dir().join(format!(
            "leveler-goal-checkpoint-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("state.db");
        let (goal, created) = {
            let db = Database::connect(&path).await.unwrap();
            let (goal, _, session) = seeded_goals(&db).await;
            let created = db
                .create(
                    new_checkpoint(&goal, &session, CheckpointReason::ContextCompaction, 42),
                    leveler_core::now(),
                )
                .await
                .unwrap();
            (goal, created)
        };
        let db = Database::connect(&path).await.unwrap();
        let loaded = db.latest_for_goal(&goal).await.unwrap().unwrap();
        assert_eq!(
            loaded, created,
            "the checkpoint must survive the connection"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    /// Fail closed: a row from a future writer is never trusted. The
    /// previous valid checkpoint wins for `latest`, and `get` names the
    /// problem instead of guessing.
    #[tokio::test]
    async fn a_future_schema_version_is_refused_not_guessed() {
        let db = Database::connect_in_memory().await.unwrap();
        let (goal, _, session) = seeded_goals(&db).await;
        let valid = db
            .create(
                new_checkpoint(&goal, &session, CheckpointReason::Manual, 10),
                leveler_core::now(),
            )
            .await
            .unwrap();
        sqlx::query(
            "INSERT INTO goal_checkpoints \
             (id, goal_id, session_id, reason, event_cursor, schema_version, payload, created_at) \
             VALUES ('future', ?1, ?2, 'manual', 20, ?3, '{}', ?4)",
        )
        .bind(goal.as_str())
        .bind(session.as_str())
        .bind(GOAL_CHECKPOINT_SCHEMA_VERSION + 1)
        .bind(leveler_core::now().to_rfc3339())
        .execute(db.pool())
        .await
        .unwrap();

        let latest = db.latest_for_goal(&goal).await.unwrap().unwrap();
        assert_eq!(latest.id, valid.id, "the previous valid checkpoint wins");
        assert!(
            GoalCheckpointStore::get(&db, &GoalCheckpointId::new("future"))
                .await
                .is_err(),
            "a strict read must refuse a future version"
        );
    }

    /// A malformed payload row is skipped, not trusted and not fatal to the
    /// goal's other checkpoints.
    #[tokio::test]
    async fn a_malformed_payload_is_skipped_for_latest() {
        let db = Database::connect_in_memory().await.unwrap();
        let (goal, _, session) = seeded_goals(&db).await;
        let valid = db
            .create(
                new_checkpoint(&goal, &session, CheckpointReason::Manual, 10),
                leveler_core::now(),
            )
            .await
            .unwrap();
        sqlx::query(
            "INSERT INTO goal_checkpoints \
             (id, goal_id, session_id, reason, event_cursor, schema_version, payload, created_at) \
             VALUES ('broken', ?1, ?2, 'manual', 20, 1, 'not json', ?3)",
        )
        .bind(goal.as_str())
        .bind(session.as_str())
        .bind(leveler_core::now().to_rfc3339())
        .execute(db.pool())
        .await
        .unwrap();
        let latest = db.latest_for_goal(&goal).await.unwrap().unwrap();
        assert_eq!(latest.id, valid.id);
    }

    /// Deleting a session cascades away its checkpoints — no orphans.
    #[tokio::test]
    async fn checkpoints_cascade_with_their_goal() {
        let db = Database::connect_in_memory().await.unwrap();
        let (goal, _, session) = seeded_goals(&db).await;
        db.create(
            new_checkpoint(&goal, &session, CheckpointReason::Manual, 1),
            leveler_core::now(),
        )
        .await
        .unwrap();
        SessionRepository::new(&db).delete(&session).await.unwrap();
        assert!(db.for_goal(&goal).await.unwrap().is_empty());
    }
}
