//! Durable queue for asynchronous semantic-memory consolidation.

use leveler_core::{BootId, Timestamp};

use crate::{Database, StorageError};

/// One claimed fresh user/chat turn. `turn_payload` is the authoritative
/// write-ahead payload from `turns`; the inbox never copies user text.
#[derive(Debug, Clone, PartialEq, Eq, sqlx::FromRow)]
pub struct MemoryInboxItem {
    /// Monotonic admission order across the database.
    pub inbox_id: i64,
    /// Source turn whose initiating message is to be consolidated.
    pub turn_id: String,
    /// Owning session.
    pub session_id: String,
    /// Turn order within the session.
    pub turn_ordinal: i64,
    /// Versioned initiating-message JSON from `turns.payload`.
    pub turn_payload: String,
    /// Model selected for the owning session.
    pub model: String,
    /// Number of claims including this one.
    pub attempt: i64,
    /// Runtime-validated staged result, if extraction already succeeded.
    pub result_json: Option<String>,
}

/// Read-only scheduling facts for work that could be claimed now.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MemoryInboxReadyState {
    /// Number of pending or due retry rows. Zero while another batch is processing.
    pub count: u64,
    /// Admission time of the oldest ready row, or `None` when `count` is zero.
    pub oldest_created_at: Option<Timestamp>,
}

/// Durable queue counts for diagnostics and worker acceptance checks.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct MemoryInboxCounts {
    /// Fresh rows not yet claimed.
    pub pending: u64,
    /// Rows currently owned by a live or not-yet-recovered boot.
    pub processing: u64,
    /// Failed rows waiting for their next attempt.
    pub failed_retryable: u64,
    /// Successfully settled rows retained for bounded diagnostics.
    pub processed: u64,
}

/// Durable inbox operations. Claims are fenced by a per-process boot id.
pub struct MemoryInboxRepository<'a> {
    db: &'a Database,
}

impl<'a> MemoryInboxRepository<'a> {
    /// Borrow `db` for this repository handle.
    pub fn new(db: &'a Database) -> Self {
        Self { db }
    }

    /// Count rows by durable processing state.
    pub async fn counts(&self) -> Result<MemoryInboxCounts, StorageError> {
        let rows: Vec<(String, i64)> =
            sqlx::query_as("SELECT status, COUNT(*) FROM memory_inbox GROUP BY status")
                .fetch_all(self.db.pool())
                .await?;
        let mut counts = MemoryInboxCounts::default();
        for (status, count) in rows {
            let count = u64::try_from(count).map_err(|_| {
                StorageError::InvalidData("memory inbox status count is negative".to_string())
            })?;
            match status.as_str() {
                "pending" => counts.pending = count,
                "processing" => counts.processing = count,
                "failed_retryable" => counts.failed_retryable = count,
                "processed" => counts.processed = count,
                _ => {
                    return Err(StorageError::InvalidData(format!(
                        "unknown memory inbox status `{status}`"
                    )));
                }
            }
        }
        Ok(counts)
    }

    /// Inspect currently claimable work without acquiring it. Uses the same
    /// due-time and project-singleton gate as [`Self::claim_batch`].
    pub async fn ready_state(&self, now: Timestamp) -> Result<MemoryInboxReadyState, StorageError> {
        let (count, oldest): (i64, Option<String>) = sqlx::query_as(
            "WITH ordered AS ( \
                 SELECT memory_inbox.id, memory_inbox.created_at, \
                        CASE WHEN (memory_inbox.next_attempt_at IS NULL \
                                        OR memory_inbox.next_attempt_at <= ?1) \
                                  AND turns.status != 'running' \
                                  AND turns.finished_at IS NOT NULL \
                             THEN 1 ELSE 0 END AS ready, \
                        ROW_NUMBER() OVER (ORDER BY memory_inbox.id) AS position \
                 FROM memory_inbox \
                 JOIN turns ON turns.id = memory_inbox.turn_id \
                 WHERE memory_inbox.status IN ('pending', 'failed_retryable') \
             ), ready_prefix AS ( \
                 SELECT current.id, current.created_at \
                 FROM ordered AS current \
                 WHERE current.ready = 1 \
                   AND NOT EXISTS ( \
                       SELECT 1 FROM ordered AS prior \
                       WHERE prior.position < current.position AND prior.ready = 0 \
                   ) \
             ) \
             SELECT COUNT(*), MIN(created_at) FROM ready_prefix \
             WHERE NOT EXISTS ( \
                 SELECT 1 FROM memory_inbox AS active \
                 WHERE active.status = 'processing' \
             )",
        )
        .bind(now.to_rfc3339())
        .fetch_one(self.db.pool())
        .await?;
        let count = u64::try_from(count).map_err(|_| {
            StorageError::InvalidData("memory inbox ready count is negative".to_string())
        })?;
        let oldest_created_at = oldest
            .map(|value| {
                value.parse::<Timestamp>().map_err(|error| {
                    StorageError::InvalidData(format!(
                        "memory inbox created_at is not RFC 3339: {error}"
                    ))
                })
            })
            .transpose()?;
        Ok(MemoryInboxReadyState {
            count,
            oldest_created_at,
        })
    }

    /// Atomically claim at most `limit` ready rows in durable admission order.
    pub async fn claim_batch(
        &self,
        boot: &BootId,
        now: Timestamp,
        limit: u32,
    ) -> Result<Vec<MemoryInboxItem>, StorageError> {
        if limit == 0 {
            return Ok(Vec::new());
        }
        let now = now.to_rfc3339();
        // Only the ready prefix of the durable queue is claimable. A failed
        // older turn therefore cannot retry after a newer correction and
        // restore stale memory. Within that prefix, the oldest row selects the
        // model and staging phase so batches remain homogeneous.
        let mut rows = sqlx::query_as::<_, MemoryInboxItem>(
            "WITH ordered AS ( \
                 SELECT memory_inbox.id, memory_inbox.model, \
                        memory_inbox.result_json IS NULL AS needs_extraction, \
                        CASE WHEN (memory_inbox.next_attempt_at IS NULL \
                                        OR memory_inbox.next_attempt_at <= ?2) \
                                  AND turns.status != 'running' \
                                  AND turns.finished_at IS NOT NULL \
                             THEN 1 ELSE 0 END AS ready, \
                        ROW_NUMBER() OVER (ORDER BY memory_inbox.id) AS position \
                 FROM memory_inbox \
                 JOIN turns ON turns.id = memory_inbox.turn_id \
                 WHERE memory_inbox.status IN ('pending', 'failed_retryable') \
             ), ready_prefix AS ( \
                 SELECT current.id, current.model, current.needs_extraction, current.position \
                 FROM ordered AS current \
                 WHERE current.ready = 1 \
                   AND NOT EXISTS ( \
                       SELECT 1 FROM ordered AS prior \
                       WHERE prior.position < current.position AND prior.ready = 0 \
                   ) \
             ), oldest_work AS ( \
                 SELECT model, needs_extraction \
                 FROM ready_prefix \
                 WHERE NOT EXISTS ( \
                     SELECT 1 FROM memory_inbox AS active \
                     WHERE active.status = 'processing' \
                 ) \
                 ORDER BY position \
                 LIMIT 1 \
             ), eligible AS ( \
                 SELECT current.id \
                 FROM ready_prefix AS current \
                 WHERE current.model = (SELECT model FROM oldest_work) \
                   AND current.needs_extraction = (SELECT needs_extraction FROM oldest_work) \
                   AND NOT EXISTS ( \
                       SELECT 1 FROM ready_prefix AS prior \
                       WHERE prior.position < current.position \
                         AND (prior.model != (SELECT model FROM oldest_work) \
                              OR prior.needs_extraction != \
                                 (SELECT needs_extraction FROM oldest_work)) \
                   ) \
                 ORDER BY current.position \
                 LIMIT ?3 \
             ) \
             UPDATE memory_inbox \
             SET status = 'processing', attempt = attempt + 1, next_attempt_at = NULL, \
                 processing_boot_id = ?1, last_error = NULL, updated_at = ?2 \
             WHERE id IN (SELECT id FROM eligible) \
             RETURNING id AS inbox_id, turn_id, \
                 (SELECT session_id FROM turns WHERE turns.id = memory_inbox.turn_id) AS session_id, \
                 (SELECT ordinal FROM turns WHERE turns.id = memory_inbox.turn_id) AS turn_ordinal, \
                 (SELECT payload FROM turns WHERE turns.id = memory_inbox.turn_id) AS turn_payload, \
                 model, \
                 attempt, result_json",
        )
        .bind(boot.as_str())
        .bind(&now)
        .bind(i64::from(limit))
        .fetch_all(self.db.pool())
        .await?;
        rows.sort_by_key(|row| row.inbox_id);
        Ok(rows)
    }

    /// Durably stage the validated result before any Memory Store mutation.
    /// A retry then replays identical runtime-approved data rather than asking
    /// a nondeterministic model to reconstruct the batch.
    pub async fn stage_results(
        &self,
        boot: &BootId,
        results: &[(i64, String)],
        now: Timestamp,
    ) -> Result<(), StorageError> {
        if results.is_empty() {
            return Ok(());
        }
        let mut transaction = self.db.pool().begin().await?;
        for (id, result_json) in results {
            let changed = sqlx::query(
                "UPDATE memory_inbox SET result_json = ?3, updated_at = ?4 \
                 WHERE id = ?1 AND status = 'processing' \
                   AND processing_boot_id = ?2 AND result_json IS NULL",
            )
            .bind(id)
            .bind(boot.as_str())
            .bind(result_json)
            .bind(now.to_rfc3339())
            .execute(&mut *transaction)
            .await?;
            if changed.rows_affected() != 1 {
                return Err(StorageError::InvalidData(format!(
                    "memory inbox claim {id} cannot stage a result for boot {}",
                    boot.as_str()
                )));
            }
        }
        transaction.commit().await?;
        Ok(())
    }

    /// Advance the opaque staged apply cursor after one candidate mutation is
    /// durable. The worker writes the whole staged value so storage remains
    /// ignorant of the domain schema.
    pub async fn update_staged_result(
        &self,
        boot: &BootId,
        inbox_id: i64,
        result_json: &str,
        now: Timestamp,
    ) -> Result<(), StorageError> {
        let changed = sqlx::query(
            "UPDATE memory_inbox SET result_json = ?3, updated_at = ?4 \
             WHERE id = ?1 AND status = 'processing' \
               AND processing_boot_id = ?2 AND result_json IS NOT NULL",
        )
        .bind(inbox_id)
        .bind(boot.as_str())
        .bind(result_json)
        .bind(now.to_rfc3339())
        .execute(self.db.pool())
        .await?;
        if changed.rows_affected() != 1 {
            return Err(StorageError::InvalidData(format!(
                "memory inbox claim {inbox_id} cannot advance staged result for boot {}",
                boot.as_str()
            )));
        }
        Ok(())
    }

    /// Mark an owned claim processed. The whole batch commits or none of it
    /// does; a missing/stolen row is persisted-data/authority mismatch.
    pub async fn mark_processed(
        &self,
        boot: &BootId,
        inbox_ids: &[i64],
        now: Timestamp,
    ) -> Result<(), StorageError> {
        self.settle_claims(boot, inbox_ids, "processed", None, None, now)
            .await
    }

    /// Return an owned claim to the retry queue at an explicit time.
    pub async fn mark_retryable(
        &self,
        boot: &BootId,
        inbox_ids: &[i64],
        next_attempt: Timestamp,
        error: &str,
        now: Timestamp,
    ) -> Result<(), StorageError> {
        self.settle_claims(
            boot,
            inbox_ids,
            "failed_retryable",
            Some(next_attempt),
            Some(error),
            now,
        )
        .await
    }

    async fn settle_claims(
        &self,
        boot: &BootId,
        inbox_ids: &[i64],
        status: &str,
        next_attempt: Option<Timestamp>,
        error: Option<&str>,
        now: Timestamp,
    ) -> Result<(), StorageError> {
        if inbox_ids.is_empty() {
            return Ok(());
        }
        let now = now.to_rfc3339();
        let next_attempt = next_attempt.map(|value| value.to_rfc3339());
        let mut transaction = self.db.pool().begin().await?;
        for id in inbox_ids {
            let changed = sqlx::query(
                "UPDATE memory_inbox \
                 SET status = ?3, next_attempt_at = ?4, processing_boot_id = NULL, \
                     last_error = ?5, updated_at = ?6, \
                     processed_at = CASE WHEN ?3 = 'processed' THEN ?6 ELSE NULL END \
                 WHERE id = ?1 AND status = 'processing' AND processing_boot_id = ?2",
            )
            .bind(id)
            .bind(boot.as_str())
            .bind(status)
            .bind(next_attempt.as_deref())
            .bind(error)
            .bind(&now)
            .execute(&mut *transaction)
            .await?;
            if changed.rows_affected() != 1 {
                return Err(StorageError::InvalidData(format!(
                    "memory inbox claim {id} is not processing for boot {}",
                    boot.as_str()
                )));
            }
        }
        transaction.commit().await?;
        Ok(())
    }

    /// Boots currently named by processing rows. The app owns boot-liveness
    /// proof and may pass only a proven-dead boot to recovery.
    pub async fn processing_boots(&self) -> Result<Vec<BootId>, StorageError> {
        let rows: Vec<String> = sqlx::query_scalar(
            "SELECT DISTINCT processing_boot_id FROM memory_inbox \
             WHERE status = 'processing' ORDER BY processing_boot_id",
        )
        .fetch_all(self.db.pool())
        .await?;
        Ok(rows.into_iter().map(BootId::new).collect())
    }

    /// Recover every claim belonging to one boot that the app has proven
    /// dead. Recovery is immediately retryable and does not increment attempt.
    pub async fn recover_claims_for_boot(
        &self,
        dead_boot: &BootId,
        reason: &str,
        now: Timestamp,
    ) -> Result<u64, StorageError> {
        let now = now.to_rfc3339();
        let changed = sqlx::query(
            "UPDATE memory_inbox \
             SET status = 'failed_retryable', next_attempt_at = ?3, \
                 processing_boot_id = NULL, last_error = ?2, updated_at = ?3 \
             WHERE status = 'processing' AND processing_boot_id = ?1",
        )
        .bind(dead_boot.as_str())
        .bind(reason)
        .bind(&now)
        .execute(self.db.pool())
        .await?;
        Ok(changed.rows_affected())
    }

    /// Delete a bounded number of processed rows older than `before`.
    pub async fn prune_processed(
        &self,
        before: Timestamp,
        limit: u32,
    ) -> Result<u64, StorageError> {
        if limit == 0 {
            return Ok(0);
        }
        let changed = sqlx::query(
            "DELETE FROM memory_inbox WHERE id IN ( \
                 SELECT id FROM memory_inbox \
                 WHERE status = 'processed' AND processed_at < ?1 \
                 ORDER BY id LIMIT ?2 \
             )",
        )
        .bind(before.to_rfc3339())
        .bind(i64::from(limit))
        .execute(self.db.pool())
        .await?;
        Ok(changed.rows_affected())
    }
}

#[cfg(test)]
mod tests {
    use std::time::Instant;

    use chrono::Duration;
    use leveler_core::{BootId, SessionId};

    use super::*;
    use crate::{SessionRecord, SessionRepository, TurnRepository};

    async fn fixture() -> (Database, SessionId) {
        let db = Database::connect_in_memory().await.unwrap();
        let record = SessionRecord::new("/repo", "goal", "provider/model", leveler_core::now());
        let session = SessionId::new(record.id.clone());
        SessionRepository::new(&db).create(&record).await.unwrap();
        (db, session)
    }

    async fn finish_all_turns(db: &Database, now: Timestamp) {
        sqlx::query(
            "UPDATE turns SET status = 'completed', finished_at = ?1 WHERE status = 'running'",
        )
        .bind(now.to_rfc3339())
        .execute(db.pool())
        .await
        .unwrap();
    }

    fn fresh_payload(message: &str) -> String {
        serde_json::json!({
            "version": 1,
            "initiating_message": { "role": "user", "content": message }
        })
        .to_string()
    }

    #[tokio::test]
    async fn turn_insert_atomically_admits_only_fresh_user_or_chat_payloads() {
        let (db, session) = fixture().await;
        let turns = TurnRepository::new(&db);
        let now = leveler_core::now();
        let user = turns
            .start(
                &session,
                "user",
                Some(r#"{"version":1,"initiating_message":{"role":"user","content":"one"}}"#),
                now,
            )
            .await
            .unwrap();
        let chat = turns
            .start(
                &session,
                "chat",
                Some(r#"{"version":1,"initiating_message":{"role":"user","content":"two"}}"#),
                now,
            )
            .await
            .unwrap();
        turns.start(&session, "user", None, now).await.unwrap();
        turns
            .start(
                &session,
                "chat",
                Some(
                    r#"{"version":2,"objective":{"text":"resume","version":1,"source":"this_turn_user"},"continuation_root_turn_id":"root"}"#,
                ),
                now,
            )
            .await
            .unwrap();
        turns
            .start(&session, "node", Some(r#"{"node_id":"n"}"#), now)
            .await
            .unwrap();

        let admitted: Vec<(String, String)> =
            sqlx::query_as("SELECT turn_id, status FROM memory_inbox ORDER BY id")
                .fetch_all(db.pool())
                .await
                .unwrap();
        assert_eq!(
            admitted,
            vec![(user.id, "pending".into()), (chat.id, "pending".into())]
        );
    }

    #[tokio::test]
    async fn economy_turn_is_not_admitted_to_the_memory_inbox() {
        let (db, session) = fixture().await;
        SessionRepository::new(&db)
            .set_axes(&session, "chat", "economy", leveler_core::now())
            .await
            .unwrap();

        TurnRepository::new(&db)
            .start(
                &session,
                "user",
                Some(r#"{"version":1,"initiating_message":{"role":"user","content":"secret preference"}}"#),
                leveler_core::now(),
            )
            .await
            .unwrap();

        let admitted: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM memory_inbox")
            .fetch_one(db.pool())
            .await
            .unwrap();
        assert_eq!(admitted, 0);
    }

    #[tokio::test]
    async fn claim_is_bounded_ordered_and_joins_authoritative_turn_and_session_data() {
        let (db, session) = fixture().await;
        let turns = TurnRepository::new(&db);
        let now = leveler_core::now();
        let first = turns
            .start(&session, "user", Some(&fresh_payload("one")), now)
            .await
            .unwrap();
        turns
            .start(
                &session,
                "chat",
                Some(&fresh_payload("two")),
                now + Duration::seconds(1),
            )
            .await
            .unwrap();
        finish_all_turns(&db, now + Duration::seconds(2)).await;

        let claimed = MemoryInboxRepository::new(&db)
            .claim_batch(&BootId::new("boot-a"), now, 1)
            .await
            .unwrap();
        assert_eq!(claimed.len(), 1);
        assert_eq!(claimed[0].turn_id, first.id);
        assert_eq!(claimed[0].session_id, session.as_str());
        assert_eq!(claimed[0].turn_ordinal, 1);
        assert_eq!(claimed[0].turn_payload, fresh_payload("one"));
        assert_eq!(claimed[0].model, "provider/model");
        assert_eq!(claimed[0].attempt, 1);
    }

    #[tokio::test]
    async fn claim_keeps_model_batches_contiguous_without_skipping_queue_order() {
        let (db, first_session) = fixture().await;
        let other = SessionRecord::new("/repo", "goal", "other/model", leveler_core::now());
        let other_session = SessionId::new(other.id.clone());
        SessionRepository::new(&db).create(&other).await.unwrap();
        let now = leveler_core::now();
        let first = TurnRepository::new(&db)
            .start(&first_session, "user", Some(&fresh_payload("a")), now)
            .await
            .unwrap();
        let second = TurnRepository::new(&db)
            .start(&other_session, "user", Some(&fresh_payload("b")), now)
            .await
            .unwrap();
        let third = TurnRepository::new(&db)
            .start(&first_session, "chat", Some(&fresh_payload("c")), now)
            .await
            .unwrap();
        finish_all_turns(&db, now).await;

        let repo = MemoryInboxRepository::new(&db);
        let claimed = repo
            .claim_batch(&BootId::new("boot-a"), now, 10)
            .await
            .unwrap();
        assert_eq!(claimed.len(), 1);
        assert_eq!(claimed[0].turn_id, first.id);
        assert!(claimed.iter().all(|item| item.model == "provider/model"));
        let pending: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM memory_inbox WHERE status = 'pending'")
                .fetch_one(db.pool())
                .await
                .unwrap();
        assert_eq!(pending, 2);

        repo.mark_processed(&BootId::new("boot-a"), &[claimed[0].inbox_id], now)
            .await
            .unwrap();
        let next = repo
            .claim_batch(&BootId::new("boot-b"), now, 10)
            .await
            .unwrap();
        assert_eq!(next.len(), 1);
        assert_eq!(next[0].turn_id, second.id);
        repo.mark_processed(&BootId::new("boot-b"), &[next[0].inbox_id], now)
            .await
            .unwrap();
        let last = repo
            .claim_batch(&BootId::new("boot-c"), now, 10)
            .await
            .unwrap();
        assert_eq!(last.len(), 1);
        assert_eq!(last[0].turn_id, third.id);
    }

    #[tokio::test]
    async fn admission_freezes_the_turn_model_even_if_the_session_model_changes() {
        let (db, session) = fixture().await;
        let now = leveler_core::now();
        TurnRepository::new(&db)
            .start(&session, "user", Some(&fresh_payload("a")), now)
            .await
            .unwrap();
        finish_all_turns(&db, now).await;
        sqlx::query("UPDATE sessions SET model = 'provider/new-model' WHERE id = ?1")
            .bind(session.as_str())
            .execute(db.pool())
            .await
            .unwrap();

        let claimed = MemoryInboxRepository::new(&db)
            .claim_batch(&BootId::new("boot-a"), now, 10)
            .await
            .unwrap();
        assert_eq!(claimed.len(), 1);
        assert_eq!(claimed[0].model, "provider/model");
    }

    #[tokio::test]
    async fn retry_waits_until_due_and_processed_rows_can_be_pruned() {
        let (db, session) = fixture().await;
        let now = leveler_core::now();
        TurnRepository::new(&db)
            .start(&session, "user", Some(&fresh_payload("a")), now)
            .await
            .unwrap();
        finish_all_turns(&db, now).await;
        let repo = MemoryInboxRepository::new(&db);
        let boot = BootId::new("boot-a");
        let first = repo.claim_batch(&boot, now, 10).await.unwrap();
        let ids = first.iter().map(|item| item.inbox_id).collect::<Vec<_>>();
        repo.mark_retryable(
            &boot,
            &ids,
            now + Duration::minutes(5),
            "provider timeout",
            now,
        )
        .await
        .unwrap();
        assert!(
            repo.claim_batch(&boot, now + Duration::minutes(4), 10)
                .await
                .unwrap()
                .is_empty()
        );
        let retry = repo
            .claim_batch(&boot, now + Duration::minutes(5), 10)
            .await
            .unwrap();
        assert_eq!(retry[0].attempt, 2);
        repo.mark_processed(&boot, &ids, now + Duration::minutes(6))
            .await
            .unwrap();
        assert_eq!(
            repo.prune_processed(now + Duration::minutes(7), 10)
                .await
                .unwrap(),
            1
        );
    }

    #[tokio::test]
    async fn only_the_claiming_boot_can_settle_and_a_dead_boot_can_be_recovered() {
        let (db, session) = fixture().await;
        let now = leveler_core::now();
        TurnRepository::new(&db)
            .start(&session, "user", Some(&fresh_payload("a")), now)
            .await
            .unwrap();
        finish_all_turns(&db, now).await;
        let repo = MemoryInboxRepository::new(&db);
        let dead = BootId::new("dead-boot");
        let claim = repo.claim_batch(&dead, now, 10).await.unwrap();
        let ids = [claim[0].inbox_id];
        assert!(
            repo.mark_processed(&BootId::new("other-boot"), &ids, now)
                .await
                .is_err()
        );
        assert_eq!(repo.processing_boots().await.unwrap(), vec![dead.clone()]);
        assert_eq!(
            repo.recover_claims_for_boot(&dead, "boot ended", now)
                .await
                .unwrap(),
            1
        );
        let reclaimed = repo
            .claim_batch(&BootId::new("new-boot"), now, 10)
            .await
            .unwrap();
        assert_eq!(reclaimed[0].attempt, 2);
    }

    #[tokio::test]
    async fn staged_result_survives_claim_recovery_for_exact_replay() {
        let (db, session) = fixture().await;
        let now = leveler_core::now();
        TurnRepository::new(&db)
            .start(&session, "user", Some(&fresh_payload("a")), now)
            .await
            .unwrap();
        finish_all_turns(&db, now).await;
        let repo = MemoryInboxRepository::new(&db);
        let old_boot = BootId::new("old-boot");
        let claim = repo.claim_batch(&old_boot, now, 1).await.unwrap();
        repo.stage_results(
            &old_boot,
            &[(claim[0].inbox_id, r#"{"accepted":[]}"#.to_string())],
            now,
        )
        .await
        .unwrap();
        repo.recover_claims_for_boot(&old_boot, "restart", now)
            .await
            .unwrap();

        let replay = repo
            .claim_batch(&BootId::new("new-boot"), now, 1)
            .await
            .unwrap();
        assert_eq!(replay[0].result_json.as_deref(), Some(r#"{"accepted":[]}"#));
    }

    #[tokio::test]
    async fn exact_replay_rows_are_not_claimed_with_fresh_extraction_rows() {
        let (db, session) = fixture().await;
        let now = leveler_core::now();
        let turns = TurnRepository::new(&db);
        for text in ["first", "second"] {
            turns
                .start(&session, "user", Some(&fresh_payload(text)), now)
                .await
                .unwrap();
        }
        finish_all_turns(&db, now).await;
        let repo = MemoryInboxRepository::new(&db);
        let old_boot = BootId::new("old-boot");
        let first = repo.claim_batch(&old_boot, now, 1).await.unwrap();
        repo.stage_results(
            &old_boot,
            &[(first[0].inbox_id, r#"{"accepted":[]}"#.to_string())],
            now,
        )
        .await
        .unwrap();
        repo.recover_claims_for_boot(&old_boot, "restart", now)
            .await
            .unwrap();

        let replay = repo
            .claim_batch(&BootId::new("new-boot"), now, 8)
            .await
            .unwrap();
        assert_eq!(replay.len(), 1);
        assert!(replay[0].result_json.is_some());
    }

    #[tokio::test]
    async fn a_project_has_only_one_processing_batch_until_it_is_settled() {
        let (db, session) = fixture().await;
        let now = leveler_core::now();
        let turns = TurnRepository::new(&db);
        for message in ["a", "b"] {
            turns
                .start(&session, "user", Some(&fresh_payload(message)), now)
                .await
                .unwrap();
        }
        finish_all_turns(&db, now).await;
        let repo = MemoryInboxRepository::new(&db);
        let boot_a = BootId::new("boot-a");
        let boot_b = BootId::new("boot-b");
        let first = repo.claim_batch(&boot_a, now, 1).await.unwrap();
        assert_eq!(first.len(), 1);
        assert!(repo.claim_batch(&boot_b, now, 1).await.unwrap().is_empty());

        repo.mark_processed(&boot_a, &[first[0].inbox_id], now)
            .await
            .unwrap();
        let second = repo.claim_batch(&boot_b, now, 1).await.unwrap();
        assert_eq!(second.len(), 1);
        assert_ne!(second[0].inbox_id, first[0].inbox_id);
    }

    #[tokio::test]
    async fn concurrent_boot_claims_have_exactly_one_winner() {
        // A file database has a multi-connection pool, so this exercises the
        // same SQLite writer race separate processes encounter. The in-memory
        // test pool intentionally has only one connection.
        let dir = tempfile::tempdir().unwrap();
        let db = Database::connect(&dir.path().join("sessions.db"))
            .await
            .unwrap();
        let record = SessionRecord::new("/repo", "goal", "provider/model", leveler_core::now());
        let session = SessionId::new(record.id.clone());
        SessionRepository::new(&db).create(&record).await.unwrap();
        let now = leveler_core::now();
        let turns = TurnRepository::new(&db);
        for message in ["a", "b"] {
            turns
                .start(&session, "user", Some(&fresh_payload(message)), now)
                .await
                .unwrap();
        }
        finish_all_turns(&db, now).await;

        let db_a = db.clone();
        let db_b = db.clone();
        let claim_a = async move {
            MemoryInboxRepository::new(&db_a)
                .claim_batch(&BootId::new("boot-a"), now, 1)
                .await
                .unwrap()
        };
        let claim_b = async move {
            MemoryInboxRepository::new(&db_b)
                .claim_batch(&BootId::new("boot-b"), now, 1)
                .await
                .unwrap()
        };
        let (a, b) = tokio::join!(claim_a, claim_b);
        assert_eq!(usize::from(!a.is_empty()) + usize::from(!b.is_empty()), 1);
        assert_eq!(a.len() + b.len(), 1);
    }

    #[tokio::test]
    async fn a_future_retry_blocks_newer_memory_until_order_can_be_preserved() {
        let (db, session) = fixture().await;
        let now = leveler_core::now();
        let old = now - Duration::minutes(2);
        let turns = TurnRepository::new(&db);
        let old_turn = turns
            .start(&session, "user", Some(&fresh_payload("old")), old)
            .await
            .unwrap();
        let new_turn = turns
            .start(&session, "chat", Some(&fresh_payload("new")), now)
            .await
            .unwrap();
        finish_all_turns(&db, now).await;
        let repo = MemoryInboxRepository::new(&db);
        let state = repo.ready_state(now).await.unwrap();
        assert_eq!(state.count, 2);
        assert_eq!(state.oldest_created_at, Some(old));

        let boot = BootId::new("boot-a");
        let claim = repo.claim_batch(&boot, now, 1).await.unwrap();
        assert_eq!(repo.ready_state(now).await.unwrap().count, 0);
        repo.mark_retryable(
            &boot,
            &[claim[0].inbox_id],
            now + Duration::minutes(5),
            "wait",
            now,
        )
        .await
        .unwrap();

        let state = repo.ready_state(now).await.unwrap();
        assert_eq!(state.count, 0, "newer work must not pass an older retry");
        assert_eq!(state.oldest_created_at, None);
        assert!(
            repo.claim_batch(&BootId::new("boot-b"), now, 10)
                .await
                .unwrap()
                .is_empty()
        );

        let due = now + Duration::minutes(5);
        assert_eq!(repo.ready_state(due).await.unwrap().count, 2);
        let claimed = repo
            .claim_batch(&BootId::new("boot-b"), due, 10)
            .await
            .unwrap();
        assert_eq!(
            claimed
                .iter()
                .map(|item| item.turn_id.as_str())
                .collect::<Vec<_>>(),
            vec![old_turn.id.as_str(), new_turn.id.as_str()]
        );
    }

    #[tokio::test]
    async fn a_running_turn_is_not_ready_or_claimable_until_it_finishes() {
        let (db, session) = fixture().await;
        let now = leveler_core::now();
        let turn = TurnRepository::new(&db)
            .start(&session, "user", Some(&fresh_payload("a")), now)
            .await
            .unwrap();
        let repo = MemoryInboxRepository::new(&db);
        assert_eq!(repo.ready_state(now).await.unwrap().count, 0);
        assert!(
            repo.claim_batch(&BootId::new("boot-a"), now, 1)
                .await
                .unwrap()
                .is_empty()
        );

        TurnRepository::new(&db)
            .finish(
                &leveler_core::TurnId::new(turn.id),
                "completed",
                now + Duration::seconds(1),
            )
            .await
            .unwrap();
        assert_eq!(
            repo.ready_state(now + Duration::seconds(1))
                .await
                .unwrap()
                .count,
            1
        );
        assert_eq!(
            repo.claim_batch(&BootId::new("boot-a"), now + Duration::seconds(1), 1)
                .await
                .unwrap()
                .len(),
            1
        );
    }

    #[tokio::test]
    #[ignore = "local performance probe; run explicitly with --nocapture"]
    async fn inbox_admission_performance_probe() {
        async fn measure(work_profile: &str, turns: usize) -> std::time::Duration {
            let db = Database::connect_in_memory().await.unwrap();
            let mut record =
                SessionRecord::new("/repo", "goal", "provider/model", leveler_core::now());
            record.work_profile = work_profile.to_string();
            let session = SessionId::new(record.id.clone());
            SessionRepository::new(&db).create(&record).await.unwrap();
            let repo = TurnRepository::new(&db);
            let started = Instant::now();
            for index in 0..turns {
                let turn = repo
                    .start(
                        &session,
                        "user",
                        Some(&fresh_payload(&format!("turn {index}"))),
                        leveler_core::now(),
                    )
                    .await
                    .unwrap();
                repo.finish(
                    &leveler_core::TurnId::new(turn.id),
                    "completed",
                    leveler_core::now(),
                )
                .await
                .unwrap();
            }
            started.elapsed()
        }

        let turns = 500;
        let mut without_samples = Vec::new();
        let mut with_samples = Vec::new();
        for round in 0..7 {
            if round % 2 == 0 {
                without_samples.push(measure("economy", turns).await.as_micros());
                with_samples.push(measure("balanced", turns).await.as_micros());
            } else {
                with_samples.push(measure("balanced", turns).await.as_micros());
                without_samples.push(measure("economy", turns).await.as_micros());
            }
        }
        without_samples.sort_unstable();
        with_samples.sort_unstable();
        let without_inbox = without_samples[without_samples.len() / 2];
        let with_inbox = with_samples[with_samples.len() / 2];
        let delta_micros = with_inbox as i128 - without_inbox as i128;
        println!(
            "turns_per_sample={turns} samples=7 without_inbox_median_us={} with_inbox_median_us={} delta_us={} delta_per_turn_us={:.2}",
            without_inbox,
            with_inbox,
            delta_micros,
            delta_micros as f64 / turns as f64
        );
    }
}
