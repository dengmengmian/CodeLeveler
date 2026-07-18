//! Persistence for resumable session message transcripts.
//!
//! Storage stays domain-agnostic: payloads are opaque JSON strings. The app
//! layer serializes/deserializes the unified `Message` type.

use leveler_core::{SessionId, Timestamp};

use crate::database::{Database, StorageError};

/// Start an IMMEDIATE transaction: the write lock is acquired at BEGIN, so a
/// read-then-write batch cannot hit SQLite's non-waitable upgrade deadlock.
async fn begin_immediate(
    pool: &sqlx::Pool<sqlx::Sqlite>,
) -> Result<sqlx::Transaction<'static, sqlx::Sqlite>, StorageError> {
    Ok(pool.begin_with("BEGIN IMMEDIATE").await?)
}

/// Read/write access to the `session_messages` table.
pub struct MessageRepository<'a> {
    db: &'a Database,
}

impl<'a> MessageRepository<'a> {
    pub fn new(db: &'a Database) -> Self {
        Self { db }
    }

    /// Append serialized message payloads for a session, continuing the ordinal
    /// sequence. Runs in a transaction so a batch appends atomically.
    pub async fn append(
        &self,
        session_id: &SessionId,
        payloads: &[String],
        now: Timestamp,
    ) -> Result<(), StorageError> {
        if payloads.is_empty() {
            return Ok(());
        }
        let ts = now.to_rfc3339();
        // BEGIN IMMEDIATE: take the write lock upfront. A deferred BEGIN that
        // reads (MAX ordinal) and then upgrades to write deadlocks against a
        // concurrent writer (the engine's event pump) with an immediate
        // `database is locked` that no busy_timeout can wait out.
        let mut tx = begin_immediate(self.db.pool()).await?;

        let next: i64 = sqlx::query_scalar(
            "SELECT COALESCE(MAX(ordinal), -1) + 1 FROM session_messages WHERE session_id = ?1",
        )
        .bind(session_id.as_str())
        .fetch_one(&mut *tx)
        .await?;

        for (offset, payload) in payloads.iter().enumerate() {
            let redacted = leveler_core::redact_secrets(payload);
            sqlx::query(
                "INSERT INTO session_messages (session_id, ordinal, payload, created_at) \
                 VALUES (?1, ?2, ?3, ?4)",
            )
            .bind(session_id.as_str())
            .bind(next + offset as i64)
            .bind(&redacted)
            .bind(&ts)
            .execute(&mut *tx)
            .await?;
        }

        tx.commit().await?;
        Ok(())
    }

    /// Like [`MessageRepository::append`], but stamps every row with the
    /// owning turn, so resume can rebuild per-turn transcripts.
    pub async fn append_in_turn(
        &self,
        session_id: &SessionId,
        turn_id: &leveler_core::TurnId,
        payloads: &[String],
        now: Timestamp,
    ) -> Result<(), StorageError> {
        if payloads.is_empty() {
            return Ok(());
        }
        let ts = now.to_rfc3339();
        // BEGIN IMMEDIATE: take the write lock upfront. A deferred BEGIN that
        // reads (MAX ordinal) and then upgrades to write deadlocks against a
        // concurrent writer (the engine's event pump) with an immediate
        // `database is locked` that no busy_timeout can wait out.
        let mut tx = begin_immediate(self.db.pool()).await?;
        let next: i64 = sqlx::query_scalar(
            "SELECT COALESCE(MAX(ordinal), -1) + 1 FROM session_messages WHERE session_id = ?1",
        )
        .bind(session_id.as_str())
        .fetch_one(&mut *tx)
        .await?;
        for (offset, payload) in payloads.iter().enumerate() {
            let redacted = leveler_core::redact_secrets(payload);
            sqlx::query(
                "INSERT INTO session_messages (session_id, ordinal, payload, created_at, turn_id) \
                 VALUES (?1, ?2, ?3, ?4, ?5)",
            )
            .bind(session_id.as_str())
            .bind(next + offset as i64)
            .bind(&redacted)
            .bind(&ts)
            .bind(turn_id.as_str())
            .execute(&mut *tx)
            .await?;
        }
        tx.commit().await?;
        Ok(())
    }

    /// Load the payloads of one turn, in order.
    pub async fn load_for_turn(
        &self,
        session_id: &SessionId,
        turn_id: &leveler_core::TurnId,
    ) -> Result<Vec<String>, StorageError> {
        let rows: Vec<(String,)> = sqlx::query_as(
            "SELECT payload FROM session_messages \
             WHERE session_id = ?1 AND turn_id = ?2 ORDER BY ordinal ASC",
        )
        .bind(session_id.as_str())
        .bind(turn_id.as_str())
        .fetch_all(self.db.pool())
        .await?;
        Ok(rows.into_iter().map(|(p,)| p).collect())
    }

    /// Load all message payloads for a session, in order.
    pub async fn load(&self, session_id: &SessionId) -> Result<Vec<String>, StorageError> {
        let rows: Vec<(String,)> = sqlx::query_as(
            "SELECT payload FROM session_messages WHERE session_id = ?1 ORDER BY ordinal ASC",
        )
        .bind(session_id.as_str())
        .fetch_all(self.db.pool())
        .await?;
        Ok(rows.into_iter().map(|(p,)| p).collect())
    }

    /// Delete all messages at or after `keep` (0-based ordinal), truncating the
    /// transcript back to its first `keep` messages. Used by conversation
    /// rollback / checkpoint restore.
    pub async fn truncate_after(
        &self,
        session_id: &SessionId,
        keep: usize,
    ) -> Result<(), StorageError> {
        sqlx::query("DELETE FROM session_messages WHERE session_id = ?1 AND ordinal >= ?2")
            .bind(session_id.as_str())
            .bind(keep as i64)
            .execute(self.db.pool())
            .await?;
        Ok(())
    }

    /// Atomically replace the entire session transcript with `payloads`.
    /// Either the full new history is committed, or the prior history is left
    /// intact (no truncate-then-fail gap).
    pub async fn replace_all(
        &self,
        session_id: &SessionId,
        payloads: &[String],
        now: Timestamp,
    ) -> Result<(), StorageError> {
        let ts = now.to_rfc3339();
        let mut tx = begin_immediate(self.db.pool()).await?;
        sqlx::query("DELETE FROM session_messages WHERE session_id = ?1")
            .bind(session_id.as_str())
            .execute(&mut *tx)
            .await?;
        for (ordinal, payload) in payloads.iter().enumerate() {
            let redacted = leveler_core::redact_secrets(payload);
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
        tx.commit().await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session_repo::{SessionRecord, SessionRepository};

    #[tokio::test]
    async fn append_and_load_preserves_order() {
        let db = Database::connect_in_memory().await.unwrap();
        let session = SessionRecord::new("/r", "g", "m", leveler_core::now());
        SessionRepository::new(&db).create(&session).await.unwrap();
        let id = SessionId::new(session.id.clone());

        let repo = MessageRepository::new(&db);
        repo.append(&id, &["a".into(), "b".into()], leveler_core::now())
            .await
            .unwrap();
        repo.append(&id, &["c".into()], leveler_core::now())
            .await
            .unwrap();

        assert_eq!(repo.load(&id).await.unwrap(), vec!["a", "b", "c"]);
    }

    #[tokio::test]
    async fn truncate_after_rolls_back_the_transcript() {
        let db = Database::connect_in_memory().await.unwrap();
        let session = SessionRecord::new("/r", "g", "m", leveler_core::now());
        SessionRepository::new(&db).create(&session).await.unwrap();
        let id = SessionId::new(session.id.clone());

        let repo = MessageRepository::new(&db);
        repo.append(
            &id,
            &["a".into(), "b".into(), "c".into(), "d".into()],
            leveler_core::now(),
        )
        .await
        .unwrap();

        repo.truncate_after(&id, 2).await.unwrap();
        assert_eq!(repo.load(&id).await.unwrap(), vec!["a", "b"]);
    }

    #[tokio::test]
    async fn append_in_turn_stamps_turn_ownership() {
        let db = Database::connect_in_memory().await.unwrap();
        let record = crate::SessionRecord::new("/r", "g", "m", leveler_core::now());
        crate::SessionRepository::new(&db)
            .create(&record)
            .await
            .unwrap();
        let session = SessionId::new(record.id);
        let turn_a = crate::TurnRepository::new(&db)
            .start(&session, "user", None, leveler_core::now())
            .await
            .unwrap();
        let turn_b = crate::TurnRepository::new(&db)
            .start(&session, "repair", None, leveler_core::now())
            .await
            .unwrap();

        let repo = MessageRepository::new(&db);
        let a_id = leveler_core::TurnId::new(turn_a.id);
        let b_id = leveler_core::TurnId::new(turn_b.id);
        repo.append_in_turn(
            &session,
            &a_id,
            &["m1".into(), "m2".into()],
            leveler_core::now(),
        )
        .await
        .unwrap();
        repo.append_in_turn(&session, &b_id, &["m3".into()], leveler_core::now())
            .await
            .unwrap();

        // Whole-session ordering is preserved across turns…
        assert_eq!(repo.load(&session).await.unwrap(), vec!["m1", "m2", "m3"]);
        // …and each turn owns exactly its own messages.
        assert_eq!(
            repo.load_for_turn(&session, &a_id).await.unwrap(),
            vec!["m1", "m2"]
        );
        assert_eq!(
            repo.load_for_turn(&session, &b_id).await.unwrap(),
            vec!["m3"]
        );
    }

    #[tokio::test]
    async fn replace_all_is_atomic_success_path() {
        let db = Database::connect_in_memory().await.unwrap();
        let session = SessionRecord::new("/r", "g", "m", leveler_core::now());
        SessionRepository::new(&db).create(&session).await.unwrap();
        let id = SessionId::new(session.id.clone());
        let repo = MessageRepository::new(&db);
        repo.append(
            &id,
            &["a".into(), "b".into(), "c".into()],
            leveler_core::now(),
        )
        .await
        .unwrap();
        repo.replace_all(&id, &["summary-only".into()], leveler_core::now())
            .await
            .unwrap();
        assert_eq!(repo.load(&id).await.unwrap(), vec!["summary-only"]);
    }

    #[tokio::test]
    async fn every_message_write_path_redacts_json_secrets() {
        let db = Database::connect_in_memory().await.unwrap();
        let session = SessionRecord::new("/r", "g", "m", leveler_core::now());
        SessionRepository::new(&db).create(&session).await.unwrap();
        let id = SessionId::new(session.id);
        let turn = crate::TurnRepository::new(&db)
            .start(&id, "user", None, leveler_core::now())
            .await
            .unwrap();
        let turn_id = leveler_core::TurnId::new(turn.id);
        let secret = r#"{"api_key":"message-secret-value"}"#.to_string();
        let repo = MessageRepository::new(&db);

        repo.append(&id, std::slice::from_ref(&secret), leveler_core::now())
            .await
            .unwrap();
        repo.append_in_turn(
            &id,
            &turn_id,
            std::slice::from_ref(&secret),
            leveler_core::now(),
        )
        .await
        .unwrap();
        repo.replace_all(&id, &[secret], leveler_core::now())
            .await
            .unwrap();

        let stored = repo.load(&id).await.unwrap();
        assert_eq!(stored.len(), 1);
        assert!(!stored[0].contains("message-secret-value"), "{stored:?}");
        assert!(stored[0].contains("[REDACTED]"), "{stored:?}");
    }
}
