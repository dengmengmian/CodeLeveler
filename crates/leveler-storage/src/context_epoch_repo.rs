//! Atomic context-epoch cut for the destructive `/compact` transition
//! (HCH-FIX-1).
//!
//! `/compact` used to be five independent commits: `replace_all` on the
//! transcript, then four epoch events appended one by one — and the derived
//! `goal_checkpoints` rows were never touched at all. A crash (or a mere
//! append error, which only warned) between those commits left a rewritten
//! transcript under plan/progress/evidence state — and durable checkpoints —
//! from the destroyed epoch, with nothing on restart able to notice. Once
//! the new conversation grew past an old checkpoint's transcript watermark,
//! resume would silently splice an old-epoch checkpoint block onto a
//! wrong-epoch delta.
//!
//! This repository makes the whole semantic transition ONE SQLite
//! transaction: replace the transcript, append the epoch events (sequences
//! assigned in-transaction, in order), and delete the session's derived
//! checkpoints. All-or-nothing — after a failure the pre-compact state is
//! intact and coherent, never mixed.
//!
//! Deleting the checkpoints is consistent with durable-history semantics:
//! a `GoalCheckpoint` is a DERIVED projection (the append-only event log
//! keeps the `GoalCheckpointCreated` record), and `/compact` is the user's
//! explicit request to collapse this session's history. A checkpoint whose
//! `transcript_ordinal` indexes a destroyed transcript must never become
//! eligible again.

use leveler_core::{SessionId, Timestamp};

use crate::database::{Database, StorageError};
use crate::event_repo::EVENT_SCHEMA_VERSION;

/// One epoch event to append inside the cut: `(event_type, payload_json)`,
/// exactly the shape `EngineEvent::to_row` produces.
pub type EpochEventRow = (String, String);

impl Database {
    /// Atomically: replace the session transcript with `replacement_payloads`
    /// (ordinals from 0), append `epoch_events` in order (contiguous
    /// sequences assigned in-transaction), and delete the session's derived
    /// goal checkpoints. Commits all of it or none of it.
    pub async fn cut_context_epoch(
        &self,
        session_id: &SessionId,
        replacement_payloads: &[String],
        epoch_events: &[EpochEventRow],
        now: Timestamp,
    ) -> Result<(), StorageError> {
        let ts = now.to_rfc3339();
        // IMMEDIATE: take the write lock at BEGIN so the read-then-write
        // event sequencing below cannot hit the non-waitable upgrade deadlock.
        let mut tx = self.pool().begin_with("BEGIN IMMEDIATE").await?;

        // 1. Replace the transcript (same shape as MessageRepository::replace_all).
        sqlx::query("DELETE FROM session_messages WHERE session_id = ?1")
            .bind(session_id.as_str())
            .execute(&mut *tx)
            .await?;
        for (ordinal, payload) in replacement_payloads.iter().enumerate() {
            let redacted = crate::redact_json_payload_for_session(
                "session message",
                payload,
                Some(session_id.as_str()),
            )?;
            sqlx::query(
                "INSERT INTO session_messages (session_id, ordinal, payload, created_at) \
                 VALUES (?1, ?2, ?3, ?4)",
            )
            .bind(session_id.as_str())
            .bind(ordinal as i64)
            .bind(&redacted)
            .bind(&ts)
            .execute(&mut *tx)
            .await?;
        }

        // 2. Append the epoch events in order. Redaction refuses BEFORE any
        //    INSERT, so one bad payload rolls the whole cut back.
        for (event_type, payload) in epoch_events {
            let redacted = crate::redact_json_payload_for_session(
                &format!("event (type '{event_type}')"),
                payload,
                Some(session_id.as_str()),
            )?;
            let id = leveler_core::EventId::generate().into_inner();
            sqlx::query(
                "INSERT INTO events \
                 (id, session_id, turn_id, sequence, type, payload, created_at, schema_version) \
                 SELECT ?1, ?2, NULL, COALESCE(MAX(sequence), 0) + 1, ?3, ?4, ?5, ?6 \
                 FROM events WHERE session_id = ?2",
            )
            .bind(&id)
            .bind(session_id.as_str())
            .bind(event_type)
            .bind(&redacted)
            .bind(&ts)
            .bind(EVENT_SCHEMA_VERSION)
            .execute(&mut *tx)
            .await?;
        }

        // 3. Invalidate the derived checkpoints of the destroyed epoch.
        sqlx::query("DELETE FROM goal_checkpoints WHERE session_id = ?1")
            .bind(session_id.as_str())
            .execute(&mut *tx)
            .await?;

        tx.commit().await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use crate::{
        Database, EventRepository, GoalCheckpointStore, GoalStore, MessageRepository,
        NewGoalCheckpoint, SessionRecord, SessionRepository, TaskStore,
    };
    use leveler_core::SessionId;
    use leveler_lifecycle::{CheckpointReason, GoalCheckpoint};

    async fn seeded(db: &Database) -> (SessionId, leveler_core::GoalId) {
        let record = SessionRecord::new("/repo", "goal", "mock/m", leveler_core::now());
        SessionRepository::new(db).create(&record).await.unwrap();
        let session = SessionId::new(record.id);
        let task = db
            .ensure_for_session(&session, leveler_core::now())
            .await
            .unwrap();
        let goal = GoalStore::open(db, &task, "the goal", leveler_core::now())
            .await
            .unwrap();
        MessageRepository::new(db)
            .append(
                &session,
                &[
                    r#"{"role":"user","content":[{"type":"text","text":"old 1"}]}"#.to_string(),
                    r#"{"role":"assistant","content":[{"type":"text","text":"old 2"}]}"#
                        .to_string(),
                ],
                leveler_core::now(),
            )
            .await
            .unwrap();
        db.create(
            NewGoalCheckpoint {
                goal_id: goal.clone(),
                session_id: session.clone(),
                reason: CheckpointReason::Manual,
                event_cursor: 0,
                payload: GoalCheckpoint {
                    objective: "the goal".into(),
                    transcript_ordinal: Some(2),
                    ..Default::default()
                },
            },
            leveler_core::now(),
        )
        .await
        .unwrap();
        (session, goal)
    }

    fn epoch_events() -> Vec<(String, String)> {
        [
            (
                "context_snapshot",
                r#"{"type":"context_snapshot","payload":{"messages":[],"through_ordinal":1}}"#,
            ),
            (
                "progress_updated",
                r#"{"type":"progress_updated","payload":{"ledger":{}}}"#,
            ),
            (
                "plan_updated",
                r#"{"type":"plan_updated","payload":{"steps":[]}}"#,
            ),
            (
                "evidence_ledger_updated",
                r#"{"type":"evidence_ledger_updated","payload":{"ledger":{}}}"#,
            ),
        ]
        .map(|(t, p)| (t.to_string(), p.to_string()))
        .to_vec()
    }

    /// The whole point: transcript replacement, epoch events, and checkpoint
    /// invalidation land together.
    #[tokio::test]
    async fn a_cut_replaces_appends_and_invalidates_together() {
        let db = Database::connect_in_memory().await.unwrap();
        let (session, goal) = seeded(&db).await;

        db.cut_context_epoch(
            &session,
            &[r#"{"role":"user","content":[{"type":"text","text":"summary"}]}"#.to_string()],
            &epoch_events(),
            leveler_core::now(),
        )
        .await
        .unwrap();

        let transcript = MessageRepository::new(&db).load(&session).await.unwrap();
        assert_eq!(transcript.len(), 1, "one summary row");
        assert!(transcript[0].contains("summary"));
        let events = EventRepository::new(&db).load(&session).await.unwrap();
        assert_eq!(
            events
                .iter()
                .map(|e| e.event_type.as_str())
                .collect::<Vec<_>>(),
            vec![
                "context_snapshot",
                "progress_updated",
                "plan_updated",
                "evidence_ledger_updated"
            ],
            "epoch events in order with contiguous sequences"
        );
        assert_eq!(events.last().unwrap().sequence, 4);
        assert!(
            db.for_goal(&goal).await.unwrap().is_empty(),
            "a checkpoint against the destroyed transcript must never be eligible again"
        );
    }

    /// HCH-FIX-1 atomicity: a failure ANYWHERE inside the cut (here: a
    /// non-JSON epoch payload, refused by redaction before its INSERT) must
    /// leave the pre-compact state fully intact — transcript, events, and
    /// checkpoints. No mixed epoch is ever externally visible.
    #[tokio::test]
    async fn a_failed_cut_rolls_the_whole_transition_back() {
        let db = Database::connect_in_memory().await.unwrap();
        let (session, goal) = seeded(&db).await;

        let mut events = epoch_events();
        events[3].1 = "not json".to_string();
        let err = db
            .cut_context_epoch(
                &session,
                &[r#"{"role":"user","content":[{"type":"text","text":"summary"}]}"#.to_string()],
                &events,
                leveler_core::now(),
            )
            .await;
        assert!(err.is_err(), "a bad payload must fail the cut");

        let transcript = MessageRepository::new(&db).load(&session).await.unwrap();
        assert_eq!(transcript.len(), 2, "original transcript intact");
        assert!(transcript[0].contains("old 1"));
        assert!(
            EventRepository::new(&db)
                .load(&session)
                .await
                .unwrap()
                .is_empty(),
            "no partial epoch events"
        );
        assert_eq!(
            db.for_goal(&goal).await.unwrap().len(),
            1,
            "checkpoints untouched on rollback"
        );
    }

    /// Characterize what the OLD five-commit sequence left behind on the same
    /// failure — the mixed state FIX-1 exists to make impossible. This is the
    /// before/after proof required by the fix train: the legacy ordering
    /// (replace_all commit, then per-event appends) strands a rewritten
    /// transcript with zero epoch events and a live stale checkpoint.
    #[tokio::test]
    async fn the_legacy_sequence_left_a_mixed_epoch_behind() {
        let db = Database::connect_in_memory().await.unwrap();
        let (session, goal) = seeded(&db).await;

        // Legacy step 1: replace_all commits on its own.
        MessageRepository::new(&db)
            .replace_all(
                &session,
                &[r#"{"role":"user","content":[{"type":"text","text":"summary"}]}"#.to_string()],
                leveler_core::now(),
            )
            .await
            .unwrap();
        // Legacy step 2: the first epoch append fails (bad payload) — the
        // old code warned and carried on.
        let repo = EventRepository::new(&db);
        let failed = repo
            .append(
                &session,
                None,
                "context_snapshot",
                "not json",
                leveler_core::now(),
            )
            .await;
        assert!(failed.is_err());

        // The stranded state the audit named: new-epoch transcript, no epoch
        // events, and the old checkpoint still eligible.
        let transcript = MessageRepository::new(&db).load(&session).await.unwrap();
        assert_eq!(transcript.len(), 1, "transcript already rewritten");
        assert!(repo.load(&session).await.unwrap().is_empty());
        assert_eq!(
            db.for_goal(&goal).await.unwrap().len(),
            1,
            "stale checkpoint survives in the legacy ordering"
        );
    }

    /// Reopen: the committed cut survives the connection, coherently.
    #[tokio::test]
    async fn a_committed_cut_survives_reopen() {
        let dir = std::env::temp_dir().join(format!(
            "leveler-epoch-cut-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("state.db");
        let (session, goal) = {
            let db = Database::connect(&path).await.unwrap();
            let (session, goal) = seeded(&db).await;
            db.cut_context_epoch(
                &session,
                &[r#"{"role":"user","content":[{"type":"text","text":"summary"}]}"#.to_string()],
                &epoch_events(),
                leveler_core::now(),
            )
            .await
            .unwrap();
            (session, goal)
        };
        let db = Database::connect(&path).await.unwrap();
        assert_eq!(
            MessageRepository::new(&db)
                .load(&session)
                .await
                .unwrap()
                .len(),
            1
        );
        assert_eq!(
            EventRepository::new(&db)
                .load(&session)
                .await
                .unwrap()
                .len(),
            4
        );
        assert!(db.for_goal(&goal).await.unwrap().is_empty());
        std::fs::remove_dir_all(&dir).ok();
    }
}
