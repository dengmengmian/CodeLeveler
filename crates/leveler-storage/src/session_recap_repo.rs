//! Durable, replayable one-line review of a settled turn (`※ 回顾`).
//!
//! The review is produced by an auxiliary model call, but it is not transcript
//! context: it never enters a model request. It IS a client-visible fact about
//! a settled turn, so it lives here with the turn as its identity instead of
//! being a transient row a reconnect would drop and regenerate.
//!
//! `(session_id, turn_id)` is the primary key: one review per turn, and a
//! retried assist call cannot create a second one.

use leveler_core::{SessionId, Timestamp};

use crate::database::{Database, StorageError};

/// One durable review row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionRecapRecord {
    /// Session the settled turn belongs to.
    pub session_id: SessionId,
    /// The settled turn this review describes.
    pub turn_id: String,
    /// The one-line review text shown to the user.
    pub text: String,
    /// RFC 3339 creation time, used to place the row in replayed history.
    pub created_at: String,
}

/// Read/write access to the `session_recaps` table.
pub struct SessionRecapRepository<'a> {
    db: &'a Database,
}

impl<'a> SessionRecapRepository<'a> {
    /// Borrow `db` for this repository handle.
    pub fn new(db: &'a Database) -> Self {
        Self { db }
    }

    /// Persist one review. Returns `true` when this call created the row and
    /// `false` when the turn already had one, so a repeated assist call is
    /// idempotent by the turn identity rather than by the text.
    pub async fn record(
        &self,
        session_id: &SessionId,
        turn_id: &str,
        text: &str,
        now: Timestamp,
    ) -> Result<bool, StorageError> {
        let result = sqlx::query(
            "INSERT INTO session_recaps (session_id, turn_id, text, created_at) \
             VALUES (?1, ?2, ?3, ?4) \
             ON CONFLICT(session_id, turn_id) DO NOTHING",
        )
        .bind(session_id.as_str())
        .bind(turn_id)
        .bind(text)
        .bind(now.to_rfc3339())
        .execute(self.db.pool())
        .await?;
        Ok(result.rows_affected() > 0)
    }

    /// Every review of a session, oldest first.
    pub async fn load_for_session(
        &self,
        session_id: &SessionId,
    ) -> Result<Vec<SessionRecapRecord>, StorageError> {
        let rows = sqlx::query(
            "SELECT session_id, turn_id, text, created_at FROM session_recaps \
             WHERE session_id = ?1 ORDER BY created_at, turn_id",
        )
        .bind(session_id.as_str())
        .fetch_all(self.db.pool())
        .await?;
        use sqlx::Row;
        rows.into_iter()
            .map(|row| {
                Ok(SessionRecapRecord {
                    session_id: SessionId::new(row.try_get::<String, _>("session_id")?),
                    turn_id: row.try_get("turn_id")?,
                    text: row.try_get("text")?,
                    created_at: row.try_get("created_at")?,
                })
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session_repo::{SessionRecord, SessionRepository};
    use crate::turn_repo::TurnRepository;

    async fn db_with_turn() -> (Database, SessionId, String) {
        let db = Database::connect_in_memory().await.unwrap();
        let record = SessionRecord::new("/repo", "goal", "mock/m", leveler_core::now());
        SessionRepository::new(&db).create(&record).await.unwrap();
        let session = SessionId::new(record.id);
        let turn = TurnRepository::new(&db)
            .start(&session, "chat", None, leveler_core::now())
            .await
            .unwrap();
        (db, session, turn.id)
    }

    #[tokio::test]
    async fn a_turn_has_at_most_one_review() {
        let (db, session, turn) = db_with_turn().await;
        let repo = SessionRecapRepository::new(&db);
        assert!(
            repo.record(&session, &turn, "first", leveler_core::now())
                .await
                .unwrap()
        );
        assert!(
            !repo
                .record(&session, &turn, "second", leveler_core::now())
                .await
                .unwrap(),
            "the same turn cannot collect a second review"
        );
        let rows = repo.load_for_session(&session).await.unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].text, "first");
    }

    #[tokio::test]
    async fn reviews_are_scoped_to_their_session_and_ordered() {
        let (db, session, turn) = db_with_turn().await;
        let other = SessionRecord::new("/repo", "goal", "mock/m", leveler_core::now());
        SessionRepository::new(&db).create(&other).await.unwrap();
        let other_session = SessionId::new(other.id);
        let repo = SessionRecapRepository::new(&db);
        repo.record(&session, &turn, "mine", leveler_core::now())
            .await
            .unwrap();
        let second_turn = TurnRepository::new(&db)
            .start(&session, "chat", None, leveler_core::now())
            .await
            .unwrap();
        repo.record(&session, &second_turn.id, "later", leveler_core::now())
            .await
            .unwrap();
        let foreign_turn = TurnRepository::new(&db)
            .start(&other_session, "chat", None, leveler_core::now())
            .await
            .unwrap();
        repo.record(
            &other_session,
            &foreign_turn.id,
            "not mine",
            leveler_core::now(),
        )
        .await
        .unwrap();
        let rows = repo.load_for_session(&session).await.unwrap();
        assert_eq!(
            rows.iter().map(|row| row.text.as_str()).collect::<Vec<_>>(),
            vec!["mine", "later"]
        );
    }
}
