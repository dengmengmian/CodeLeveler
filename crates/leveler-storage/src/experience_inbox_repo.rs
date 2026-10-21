//! Durable queue for evidence-bound agent-experience extraction.
//!
//! One row is admitted per fresh user/chat turn, by a SQL trigger on the turn
//! insert. The worker claims a row only after the turn is terminal, because
//! the evidence it reads (`tool_call_started` / `tool_call_finished` /
//! `task_finished`) does not exist before then. Claims are fenced by boot id;
//! a staged result makes a retry replay identical data instead of asking the
//! model again.

use leveler_core::{BootId, Timestamp};

use crate::{Database, StorageError};

/// One claimed turn whose execution facts are to be inspected.
#[derive(Debug, Clone, PartialEq, Eq, sqlx::FromRow)]
pub struct ExperienceInboxItem {
    /// Monotonic admission order across the database.
    pub inbox_id: i64,
    /// Source turn whose execution facts are to be inspected.
    pub turn_id: String,
    /// Owning session.
    pub session_id: String,
    /// `completed` | `failed` | `interrupted`
    pub turn_status: String,
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
pub struct ExperienceInboxReadyState {
    /// Number of claimable rows. Zero while another row is processing.
    pub count: u64,
    /// Admission time of the oldest ready row, or `None` when `count` is zero.
    pub oldest_created_at: Option<Timestamp>,
}

/// Durable queue counts for diagnostics and the memory status surface.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ExperienceInboxCounts {
    /// Fresh rows not yet claimed.
    pub pending: u64,
    /// Rows currently owned by a boot.
    pub processing: u64,
    /// Failed rows waiting for their next attempt.
    pub failed_retryable: u64,
    /// Settled rows retained for bounded diagnostics.
    pub processed: u64,
}

/// One retained processed row, for the memory status surface.
#[derive(Debug, Clone, PartialEq, Eq, sqlx::FromRow)]
pub struct ExperienceInboxProcessed {
    /// When the row settled.
    pub processed_at: String,
    /// Opaque runtime-validated result. The app parses it.
    pub result_json: Option<String>,
}

/// The oldest due retry, if any.
#[derive(Debug, Clone, PartialEq, Eq, sqlx::FromRow)]
pub struct ExperienceInboxFailure {
    /// Failure class recorded by the consolidator.
    pub last_error: Option<String>,
    /// Attempts made so far.
    pub attempt: i64,
    /// When the next retry is due.
    pub next_attempt_at: Option<String>,
    /// When the row last changed.
    pub updated_at: String,
}

/// Durable experience-inbox operations.
pub struct ExperienceInboxRepository<'a> {
    db: &'a Database,
}

impl<'a> ExperienceInboxRepository<'a> {
    /// Borrow `db` for this repository handle.
    pub fn new(db: &'a Database) -> Self {
        Self { db }
    }

    /// Count rows by durable processing state.
    pub async fn counts(&self) -> Result<ExperienceInboxCounts, StorageError> {
        let rows: Vec<(String, i64)> =
            sqlx::query_as("SELECT status, COUNT(*) FROM experience_inbox GROUP BY status")
                .fetch_all(self.db.pool())
                .await?;
        let mut counts = ExperienceInboxCounts::default();
        for (status, count) in rows {
            let count = u64::try_from(count).map_err(|_| {
                StorageError::InvalidData("experience inbox status count is negative".to_string())
            })?;
            match status.as_str() {
                "pending" => counts.pending = count,
                "processing" => counts.processing = count,
                "failed_retryable" => counts.failed_retryable = count,
                "processed" => counts.processed = count,
                _ => {
                    return Err(StorageError::InvalidData(format!(
                        "unknown experience inbox status `{status}`"
                    )));
                }
            }
        }
        Ok(counts)
    }

    /// The most recently settled row, for the memory status surface.
    pub async fn last_processed(&self) -> Result<Option<ExperienceInboxProcessed>, StorageError> {
        Ok(sqlx::query_as::<_, ExperienceInboxProcessed>(
            "SELECT processed_at, result_json FROM experience_inbox \
             WHERE status = 'processed' AND processed_at IS NOT NULL \
             ORDER BY processed_at DESC, id DESC LIMIT 1",
        )
        .fetch_optional(self.db.pool())
        .await?)
    }

    /// The oldest due retry, if any.
    pub async fn last_failure(&self) -> Result<Option<ExperienceInboxFailure>, StorageError> {
        Ok(sqlx::query_as::<_, ExperienceInboxFailure>(
            "SELECT last_error, attempt, next_attempt_at, updated_at FROM experience_inbox \
             WHERE status = 'failed_retryable' \
             ORDER BY next_attempt_at ASC, id ASC LIMIT 1",
        )
        .fetch_optional(self.db.pool())
        .await?)
    }

    /// Inspect claimable work without acquiring it.
    ///
    /// A row is blocked only by an older, not-yet-ready row in the SAME
    /// session. That preserves per-session lesson order without letting one
    /// session's running turn starve every other session.
    pub async fn ready_state(
        &self,
        now: Timestamp,
    ) -> Result<ExperienceInboxReadyState, StorageError> {
        let (count, oldest): (i64, Option<String>) = sqlx::query_as(
            "WITH ordered AS ( \
                 SELECT experience_inbox.id, experience_inbox.created_at, \
                        (SELECT session_id FROM turns WHERE turns.id = experience_inbox.turn_id) AS session_id, \
                        CASE WHEN (experience_inbox.next_attempt_at IS NULL \
                                        OR experience_inbox.next_attempt_at <= ?1) \
                                  AND turns.status != 'running' \
                                  AND turns.finished_at IS NOT NULL \
                             THEN 1 ELSE 0 END AS ready \
                 FROM experience_inbox \
                 JOIN turns ON turns.id = experience_inbox.turn_id \
                 WHERE experience_inbox.status IN ('pending', 'failed_retryable') \
             ), ready AS ( \
                 SELECT current.id, current.created_at \
                 FROM ordered AS current \
                 WHERE current.ready = 1 \
                   AND NOT EXISTS ( \
                       SELECT 1 FROM ordered AS prior \
                       WHERE prior.session_id = current.session_id \
                         AND prior.id < current.id AND prior.ready = 0 \
                   ) \
             ) \
             SELECT COUNT(*), MIN(created_at) FROM ready \
             WHERE NOT EXISTS ( \
                 SELECT 1 FROM experience_inbox AS active \
                 WHERE active.status = 'processing' \
             )",
        )
        .bind(now.to_rfc3339())
        .fetch_one(self.db.pool())
        .await?;
        let count = u64::try_from(count).map_err(|_| {
            StorageError::InvalidData("experience inbox ready count is negative".to_string())
        })?;
        let oldest_created_at = oldest
            .map(|value| {
                value.parse::<Timestamp>().map_err(|error| {
                    StorageError::InvalidData(format!(
                        "experience inbox created_at is not RFC 3339: {error}"
                    ))
                })
            })
            .transpose()?;
        Ok(ExperienceInboxReadyState {
            count,
            oldest_created_at,
        })
    }

    /// Atomically claim at most `limit` ready rows in admission order.
    pub async fn claim_batch(
        &self,
        boot: &BootId,
        now: Timestamp,
        limit: u32,
    ) -> Result<Vec<ExperienceInboxItem>, StorageError> {
        if limit == 0 {
            return Ok(Vec::new());
        }
        let now = now.to_rfc3339();
        let mut rows = sqlx::query_as::<_, ExperienceInboxItem>(
            "WITH ordered AS ( \
                 SELECT experience_inbox.id, \
                        (SELECT session_id FROM turns WHERE turns.id = experience_inbox.turn_id) AS session_id, \
                        CASE WHEN (experience_inbox.next_attempt_at IS NULL \
                                        OR experience_inbox.next_attempt_at <= ?2) \
                                  AND turns.status != 'running' \
                                  AND turns.finished_at IS NOT NULL \
                             THEN 1 ELSE 0 END AS ready \
                 FROM experience_inbox \
                 JOIN turns ON turns.id = experience_inbox.turn_id \
                 WHERE experience_inbox.status IN ('pending', 'failed_retryable') \
             ), ready_prefix AS ( \
                 SELECT current.id \
                 FROM ordered AS current \
                 WHERE current.ready = 1 \
                   AND NOT EXISTS ( \
                       SELECT 1 FROM ordered AS prior \
                       WHERE prior.session_id = current.session_id \
                         AND prior.id < current.id AND prior.ready = 0 \
                   ) \
                 AND NOT EXISTS ( \
                     SELECT 1 FROM experience_inbox AS active \
                     WHERE active.status = 'processing' \
                 ) \
                 ORDER BY current.id \
                 LIMIT ?3 \
             ) \
             UPDATE experience_inbox \
             SET status = 'processing', attempt = attempt + 1, next_attempt_at = NULL, \
                 processing_boot_id = ?1, last_error = NULL, updated_at = ?2 \
             WHERE id IN (SELECT id FROM ready_prefix) \
             RETURNING id AS inbox_id, turn_id, \
                 (SELECT session_id FROM turns WHERE turns.id = experience_inbox.turn_id) AS session_id, \
                 (SELECT status FROM turns WHERE turns.id = experience_inbox.turn_id) AS turn_status, \
                 (SELECT payload FROM turns WHERE turns.id = experience_inbox.turn_id) AS turn_payload, \
                 model, attempt, result_json",
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
    pub async fn stage_result(
        &self,
        boot: &BootId,
        inbox_id: i64,
        result_json: &str,
        now: Timestamp,
    ) -> Result<(), StorageError> {
        let changed = sqlx::query(
            "UPDATE experience_inbox SET result_json = ?3, updated_at = ?4 \
             WHERE id = ?1 AND status = 'processing' \
               AND processing_boot_id = ?2 AND result_json IS NULL",
        )
        .bind(inbox_id)
        .bind(boot.as_str())
        .bind(result_json)
        .bind(now.to_rfc3339())
        .execute(self.db.pool())
        .await?;
        if changed.rows_affected() != 1 {
            return Err(StorageError::InvalidData(format!(
                "experience inbox claim {inbox_id} cannot stage a result for boot {}",
                boot.as_str()
            )));
        }
        Ok(())
    }

    /// Advance the opaque apply cursor after one candidate mutation is durable.
    pub async fn update_staged_result(
        &self,
        boot: &BootId,
        inbox_id: i64,
        result_json: &str,
        now: Timestamp,
    ) -> Result<(), StorageError> {
        let changed = sqlx::query(
            "UPDATE experience_inbox SET result_json = ?3, updated_at = ?4 \
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
                "experience inbox claim {inbox_id} cannot advance its staged result for boot {}",
                boot.as_str()
            )));
        }
        Ok(())
    }

    /// Mark an owned claim processed.
    pub async fn mark_processed(
        &self,
        boot: &BootId,
        inbox_id: i64,
        now: Timestamp,
    ) -> Result<(), StorageError> {
        self.settle_claim(boot, inbox_id, "processed", None, None, now)
            .await
    }

    /// Return an owned claim to the retry queue at an explicit time.
    pub async fn mark_retryable(
        &self,
        boot: &BootId,
        inbox_id: i64,
        next_attempt: Timestamp,
        error: &str,
        now: Timestamp,
    ) -> Result<(), StorageError> {
        self.settle_claim(
            boot,
            inbox_id,
            "failed_retryable",
            Some(next_attempt),
            Some(error),
            now,
        )
        .await
    }

    async fn settle_claim(
        &self,
        boot: &BootId,
        inbox_id: i64,
        status: &str,
        next_attempt: Option<Timestamp>,
        error: Option<&str>,
        now: Timestamp,
    ) -> Result<(), StorageError> {
        let now = now.to_rfc3339();
        let next_attempt = next_attempt.map(|value| value.to_rfc3339());
        let changed = sqlx::query(
            "UPDATE experience_inbox \
             SET status = ?3, next_attempt_at = ?4, processing_boot_id = NULL, \
                 last_error = ?5, updated_at = ?6, \
                 processed_at = CASE WHEN ?3 = 'processed' THEN ?6 ELSE NULL END \
             WHERE id = ?1 AND status = 'processing' AND processing_boot_id = ?2",
        )
        .bind(inbox_id)
        .bind(boot.as_str())
        .bind(status)
        .bind(next_attempt.as_deref())
        .bind(error)
        .bind(&now)
        .execute(self.db.pool())
        .await?;
        if changed.rows_affected() != 1 {
            return Err(StorageError::InvalidData(format!(
                "experience inbox claim {inbox_id} is not processing for boot {}",
                boot.as_str()
            )));
        }
        Ok(())
    }

    /// Boots currently named by processing rows.
    pub async fn processing_boots(&self) -> Result<Vec<BootId>, StorageError> {
        let rows: Vec<String> = sqlx::query_scalar(
            "SELECT DISTINCT processing_boot_id FROM experience_inbox \
             WHERE status = 'processing' ORDER BY processing_boot_id",
        )
        .fetch_all(self.db.pool())
        .await?;
        Ok(rows.into_iter().map(BootId::new).collect())
    }

    /// Recover every claim belonging to one proven-dead boot.
    pub async fn recover_claims_for_boot(
        &self,
        dead_boot: &BootId,
        reason: &str,
        now: Timestamp,
    ) -> Result<u64, StorageError> {
        let now = now.to_rfc3339();
        let changed = sqlx::query(
            "UPDATE experience_inbox \
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
            "DELETE FROM experience_inbox WHERE id IN ( \
                 SELECT id FROM experience_inbox \
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
    use super::*;
    use crate::{SessionRecord, SessionRepository, TurnRepository};
    use leveler_core::SessionId;

    async fn fixture() -> (Database, SessionId) {
        let db = Database::connect_in_memory().await.unwrap();
        let record = SessionRecord::new("/repo", "goal", "provider/model", leveler_core::now());
        let session = SessionId::new(record.id.clone());
        SessionRepository::new(&db).create(&record).await.unwrap();
        (db, session)
    }

    async fn admit_turn(db: &Database, session: &SessionId) -> String {
        let payload = serde_json::json!({
            "initiating_message": {"role": "user", "content": [{"type": "text", "text": "fix it"}]}
        })
        .to_string();
        let turn = TurnRepository::new(db)
            .start(session, "user", Some(&payload), leveler_core::now())
            .await
            .unwrap();
        turn.id
    }

    #[tokio::test]
    async fn a_running_turn_is_not_claimable_until_it_is_finished() {
        let (db, session) = fixture().await;
        let turn_id = admit_turn(&db, &session).await;
        let repo = ExperienceInboxRepository::new(&db);
        assert_eq!(repo.counts().await.unwrap().pending, 1);
        assert!(
            repo.claim_batch(&BootId::generate(), leveler_core::now(), 8)
                .await
                .unwrap()
                .is_empty()
        );

        TurnRepository::new(&db)
            .finish(
                &leveler_core::TurnId::new(turn_id.clone()),
                "completed",
                leveler_core::now(),
            )
            .await
            .unwrap();
        let claimed = repo
            .claim_batch(&BootId::generate(), leveler_core::now(), 8)
            .await
            .unwrap();
        assert_eq!(claimed.len(), 1);
        assert_eq!(claimed[0].turn_status, "completed");
    }

    #[tokio::test]
    async fn a_dead_boot_claim_is_recovered_and_replayable() {
        let (db, session) = fixture().await;
        let turn_id = admit_turn(&db, &session).await;
        TurnRepository::new(&db)
            .finish(
                &leveler_core::TurnId::new(turn_id),
                "completed",
                leveler_core::now(),
            )
            .await
            .unwrap();
        let repo = ExperienceInboxRepository::new(&db);
        let boot = BootId::generate();
        let claimed = repo
            .claim_batch(&boot, leveler_core::now(), 8)
            .await
            .unwrap();
        assert_eq!(claimed.len(), 1);
        let staged =
            r#"{"outcome":"extracted","accepted":[],"rejected":0,"skipped":0,"applied":0}"#;
        repo.stage_result(&boot, claimed[0].inbox_id, staged, leveler_core::now())
            .await
            .unwrap();

        repo.recover_claims_for_boot(&boot, "worker boot ended", leveler_core::now())
            .await
            .unwrap();
        let next_boot = BootId::generate();
        let again = repo
            .claim_batch(&next_boot, leveler_core::now(), 8)
            .await
            .unwrap();
        assert_eq!(again.len(), 1);
        assert_eq!(
            again[0].result_json.as_deref(),
            Some(staged),
            "the replay must not ask the model again"
        );
    }

    #[tokio::test]
    async fn a_resumed_continuation_turn_is_admitted_without_a_new_message() {
        let (db, session) = fixture().await;
        let payload = serde_json::json!({
            "version": 2,
            "initiating_message": null,
            "objective": {
                "text": "finish the interrupted fix",
                "version": 1,
                "source": "continue_active",
                "primary": "fix the check"
            }
        })
        .to_string();
        TurnRepository::new(&db)
            .start(&session, "user", Some(&payload), leveler_core::now())
            .await
            .unwrap();
        let counts = ExperienceInboxRepository::new(&db).counts().await.unwrap();
        assert_eq!(
            counts.pending, 1,
            "a resumed turn can still produce the lesson"
        );
    }

    #[tokio::test]
    async fn disabled_memory_admits_nothing() {
        let (db, session) = fixture().await;
        SessionRepository::new(&db)
            .set_memory_enabled(&session, false)
            .await
            .unwrap();
        admit_turn(&db, &session).await;
        let counts = ExperienceInboxRepository::new(&db).counts().await.unwrap();
        assert_eq!(
            counts.pending + counts.processing + counts.failed_retryable + counts.processed,
            0
        );
    }
}
