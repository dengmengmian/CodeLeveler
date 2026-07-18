//! Persistent command receipts (M5): make at-least-once command delivery safe
//! by tracking each `command_id`'s dispatch lifecycle. A re-delivery is treated
//! as a completed duplicate ONLY when the first dispatch actually succeeded; a
//! command whose `send` failed or whose process died mid-dispatch is retryable
//! or surfaced, never silently swallowed. Durable, so the lifecycle survives a
//! restart rather than being forgotten by an in-memory set.

use leveler_core::{CommandId, SessionId, Timestamp};

use crate::database::{Database, StorageError};

/// What the caller should do with an admitted command receipt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Admission {
    /// First delivery, or a retry after a failed attempt: dispatch it. The
    /// receipt is now `dispatching`; the caller must then call
    /// [`CommandReceiptRepository::mark_completed`] or `mark_failed`.
    Dispatch,
    /// A prior dispatch succeeded — a true duplicate; do not run it again.
    AlreadyCompleted,
    /// A prior attempt died while dispatching: its side effect cannot be proven
    /// done or un-done, so it must be neither silently treated as completed nor
    /// blindly re-run — the caller surfaces it.
    Uncertain,
    /// The id was already bound to another session or payload.
    Conflict,
}

/// Read/write access to the `command_receipts` table.
pub struct CommandReceiptRepository<'a> {
    db: &'a Database,
}

impl<'a> CommandReceiptRepository<'a> {
    pub fn new(db: &'a Database) -> Self {
        Self { db }
    }

    /// Classify an existing terminal receipt without creating or claiming one.
    /// `None` means either unseen or explicitly retryable (`failed`), so the
    /// caller must perform normal command validation before calling `admit`.
    pub async fn classify_terminal(
        &self,
        command_id: &CommandId,
        session_id: &SessionId,
        command_fingerprint: &str,
    ) -> Result<Option<Admission>, StorageError> {
        let row: Option<(String, String, String)> = sqlx::query_as(
            "SELECT session_id, command_fingerprint, status FROM command_receipts \
             WHERE command_id = ?1",
        )
        .bind(command_id.as_str())
        .fetch_optional(self.db.pool())
        .await?;
        let Some((stored_session, stored_fingerprint, status)) = row else {
            return Ok(None);
        };
        if stored_session != session_id.as_str() || stored_fingerprint != command_fingerprint {
            return Ok(Some(Admission::Conflict));
        }
        Ok(match status.as_str() {
            "completed" => Some(Admission::AlreadyCompleted),
            "dispatching" => Some(Admission::Uncertain),
            _ => None,
        })
    }

    /// Admit a command receipt and report what the caller should do. Durable:
    /// the lifecycle survives a restart, so a re-delivery after a crash is
    /// classified from the persisted status, not forgotten.
    pub async fn admit(
        &self,
        command_id: &CommandId,
        session_id: &SessionId,
        command_fingerprint: &str,
        issued_at: &str,
        now: Timestamp,
    ) -> Result<Admission, StorageError> {
        // First delivery: insert as 'dispatching'. rows_affected == 1 means we
        // won the PRIMARY KEY, so this id had not been seen before.
        let inserted = sqlx::query(
            "INSERT OR IGNORE INTO command_receipts \
             (command_id, session_id, command_fingerprint, issued_at, admitted_at, status) \
             VALUES (?1, ?2, ?3, ?4, ?5, 'dispatching')",
        )
        .bind(command_id.as_str())
        .bind(session_id.as_str())
        .bind(command_fingerprint)
        .bind(issued_at)
        .bind(now.to_rfc3339())
        .execute(self.db.pool())
        .await?;
        if inserted.rows_affected() == 1 {
            return Ok(Admission::Dispatch);
        }

        // Re-delivery: classify from the persisted status.
        let row: Option<(String, String, String)> = sqlx::query_as(
            "SELECT session_id, command_fingerprint, status FROM command_receipts \
             WHERE command_id = ?1",
        )
        .bind(command_id.as_str())
        .fetch_optional(self.db.pool())
        .await?;
        let Some((stored_session, stored_fingerprint, status)) = row else {
            return Ok(Admission::Uncertain);
        };
        if stored_session != session_id.as_str() || stored_fingerprint != command_fingerprint {
            return Ok(Admission::Conflict);
        }
        match status.as_str() {
            "completed" => Ok(Admission::AlreadyCompleted),
            "failed" => {
                // The prior attempt errored before doing anything — safe to
                // dispatch again. Atomically claim it: concurrent redeliveries
                // may both have observed `failed`, but only one may transition
                // failed -> dispatching and receive Dispatch.
                let claimed = sqlx::query(
                    "UPDATE command_receipts SET status = 'dispatching' \
                     WHERE command_id = ?1 AND status = 'failed'",
                )
                .bind(command_id.as_str())
                .execute(self.db.pool())
                .await?;
                if claimed.rows_affected() == 1 {
                    Ok(Admission::Dispatch)
                } else {
                    Ok(Admission::Uncertain)
                }
            }
            // 'dispatching' (or any unexpected value) = a prior attempt is still
            // in flight or died mid-dispatch: uncertain.
            _ => Ok(Admission::Uncertain),
        }
    }

    /// Mark a dispatched command as successfully completed — a later re-delivery
    /// is then a true duplicate.
    pub async fn mark_completed(&self, command_id: &CommandId) -> Result<(), StorageError> {
        self.set_status(command_id, "completed").await
    }

    /// Mark a dispatched command as failed before any effect — it is retryable.
    pub async fn mark_failed(&self, command_id: &CommandId) -> Result<(), StorageError> {
        self.set_status(command_id, "failed").await
    }

    async fn set_status(&self, command_id: &CommandId, status: &str) -> Result<(), StorageError> {
        let result = sqlx::query(
            "UPDATE command_receipts SET status = ?2 \
             WHERE command_id = ?1 AND status = 'dispatching'",
        )
        .bind(command_id.as_str())
        .bind(status)
        .execute(self.db.pool())
        .await?;
        if result.rows_affected() != 1 {
            return Err(StorageError::InvalidData(format!(
                "command receipt {} is not dispatching",
                command_id.as_str()
            )));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{SessionRecord, SessionRepository};

    async fn seed_session(db: &Database) -> SessionId {
        let record = SessionRecord::new("/repo", "goal", "mock/m", leveler_core::now());
        SessionRepository::new(db).create(&record).await.unwrap();
        SessionId::new(record.id)
    }

    #[tokio::test]
    async fn first_dispatches_then_completed_re_delivery_is_a_duplicate() {
        let db = Database::connect_in_memory().await.unwrap();
        let session = seed_session(&db).await;
        let repo = CommandReceiptRepository::new(&db);
        let id = CommandId::new("cmd-1");
        assert_eq!(
            repo.admit(&id, &session, "fp", "t", leveler_core::now())
                .await
                .unwrap(),
            Admission::Dispatch,
            "first delivery dispatches"
        );
        repo.mark_completed(&id).await.unwrap();
        assert_eq!(
            repo.admit(&id, &session, "fp", "t", leveler_core::now())
                .await
                .unwrap(),
            Admission::AlreadyCompleted,
            "a re-delivery of a completed command is a duplicate"
        );
    }

    #[tokio::test]
    async fn failed_dispatch_is_retryable_not_swallowed() {
        // The core bug: a command whose send failed must NOT be swallowed as a
        // duplicate — the same id can dispatch again.
        let db = Database::connect_in_memory().await.unwrap();
        let session = seed_session(&db).await;
        let repo = CommandReceiptRepository::new(&db);
        let id = CommandId::new("cmd-2");
        assert_eq!(
            repo.admit(&id, &session, "fp", "t", leveler_core::now())
                .await
                .unwrap(),
            Admission::Dispatch
        );
        repo.mark_failed(&id).await.unwrap();
        assert_eq!(
            repo.admit(&id, &session, "fp", "t", leveler_core::now())
                .await
                .unwrap(),
            Admission::Dispatch,
            "a failed dispatch is retryable, not a duplicate"
        );
    }

    #[tokio::test]
    async fn crash_mid_dispatch_is_uncertain_not_silently_done() {
        // Admitted but never marked (process died mid-dispatch): a re-delivery
        // must be surfaced as uncertain, not silently returned as completed.
        let db = Database::connect_in_memory().await.unwrap();
        let session = seed_session(&db).await;
        let id = CommandId::new("cmd-3");
        assert_eq!(
            CommandReceiptRepository::new(&db)
                .admit(&id, &session, "fp", "t", leveler_core::now())
                .await
                .unwrap(),
            Admission::Dispatch
        );
        // No mark_completed/mark_failed — simulate a crash. A fresh handle (a
        // restart) re-admits.
        assert_eq!(
            CommandReceiptRepository::new(&db)
                .admit(&id, &session, "fp", "t", leveler_core::now())
                .await
                .unwrap(),
            Admission::Uncertain,
            "a dispatch that never resolved is uncertain after restart"
        );
    }

    #[tokio::test]
    async fn completed_status_survives_a_restart() {
        let db = Database::connect_in_memory().await.unwrap();
        let session = seed_session(&db).await;
        let id = CommandId::new("cmd-4");
        let first = CommandReceiptRepository::new(&db)
            .admit(&id, &session, "fp", "t", leveler_core::now())
            .await
            .unwrap();
        assert_eq!(first, Admission::Dispatch);
        CommandReceiptRepository::new(&db)
            .mark_completed(&id)
            .await
            .unwrap();
        // A new handle (restart) still sees it completed.
        assert_eq!(
            CommandReceiptRepository::new(&db)
                .admit(&id, &session, "fp", "t", leveler_core::now())
                .await
                .unwrap(),
            Admission::AlreadyCompleted
        );
    }

    #[tokio::test]
    async fn concurrent_failed_redeliveries_have_one_dispatch_winner() {
        let db = Database::connect_in_memory().await.unwrap();
        let session = seed_session(&db).await;
        let id = CommandId::new("cmd-race");
        let repo = CommandReceiptRepository::new(&db);
        assert_eq!(
            repo.admit(&id, &session, "fp", "t", leveler_core::now())
                .await
                .unwrap(),
            Admission::Dispatch
        );
        repo.mark_failed(&id).await.unwrap();

        let retry_a = CommandReceiptRepository::new(&db);
        let retry_b = CommandReceiptRepository::new(&db);
        let (a, b) = tokio::join!(
            retry_a.admit(&id, &session, "fp", "t", leveler_core::now()),
            retry_b.admit(&id, &session, "fp", "t", leveler_core::now()),
        );
        let admissions = [a.unwrap(), b.unwrap()];
        assert_eq!(
            admissions
                .iter()
                .filter(|&&a| a == Admission::Dispatch)
                .count(),
            1,
            "exactly one retry may atomically claim failed -> dispatching"
        );
        assert_eq!(
            admissions
                .iter()
                .filter(|&&a| a == Admission::Uncertain)
                .count(),
            1,
            "the losing concurrent retry must observe an uncertain in-flight dispatch"
        );
    }

    #[tokio::test]
    async fn reused_command_id_with_different_identity_is_a_conflict() {
        let db = Database::connect_in_memory().await.unwrap();
        let session_a = seed_session(&db).await;
        let session_b = seed_session(&db).await;
        let id = CommandId::new("cmd-bound");
        let repo = CommandReceiptRepository::new(&db);
        assert_eq!(
            repo.admit(&id, &session_a, "fp-a", "t", leveler_core::now())
                .await
                .unwrap(),
            Admission::Dispatch
        );
        repo.mark_completed(&id).await.unwrap();
        assert_eq!(
            repo.admit(&id, &session_b, "fp-a", "t", leveler_core::now())
                .await
                .unwrap(),
            Admission::Conflict,
            "a command id cannot move to another session"
        );
        assert_eq!(
            repo.admit(&id, &session_a, "fp-b", "t", leveler_core::now())
                .await
                .unwrap(),
            Admission::Conflict,
            "a command id cannot be reused for another payload"
        );
    }

    #[tokio::test]
    async fn terminal_mark_requires_dispatching_status() {
        let db = Database::connect_in_memory().await.unwrap();
        let session = seed_session(&db).await;
        let id = CommandId::new("cmd-terminal");
        let repo = CommandReceiptRepository::new(&db);
        repo.admit(&id, &session, "fp", "t", leveler_core::now())
            .await
            .unwrap();
        repo.mark_completed(&id).await.unwrap();
        assert!(
            matches!(
                repo.mark_failed(&id).await,
                Err(StorageError::InvalidData(_))
            ),
            "a stale attempt must not overwrite a terminal receipt"
        );
    }
}
