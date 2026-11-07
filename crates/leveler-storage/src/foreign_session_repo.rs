//! Origin of a session created by importing another agent's session.
//!
//! The imported session is an ordinary native CodeLeveler session. This row
//! only says where it came from: which agent, which native id, and the exact
//! source file the read-only history tool may address. It is not a cache and
//! not a second transcript authority; every read goes back to the source file.

use leveler_core::Timestamp;

use crate::database::{Database, StorageError};

/// One persisted foreign-session origin.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ForeignSessionOriginRecord {
    /// The native CodeLeveler session created by the import.
    pub session_id: String,
    /// Source tool wire name: `claude-code` or `codex`.
    pub tool: String,
    /// The source agent's own session id.
    pub native_id: String,
    /// Absolute path of the source session file.
    pub source_path: String,
    /// Working directory recorded by the source session, when it recorded one.
    pub source_cwd: Option<String>,
    /// RFC 3339 UTC time this import was created.
    pub imported_at: String,
}

/// Reads and writes [`ForeignSessionOriginRecord`] rows.
pub struct ForeignSessionOriginRepository<'a> {
    db: &'a Database,
}

impl<'a> ForeignSessionOriginRepository<'a> {
    /// A repository over `db`.
    pub fn new(db: &'a Database) -> Self {
        Self { db }
    }

    /// Record the origin of a newly imported session. One row per session:
    /// importing twice creates two CodeLeveler sessions with two rows.
    pub async fn insert(
        &self,
        session_id: &str,
        tool: &str,
        native_id: &str,
        source_path: &str,
        source_cwd: Option<&str>,
        now: Timestamp,
    ) -> Result<(), StorageError> {
        sqlx::query(
            "INSERT INTO foreign_session_origins \
             (session_id, tool, native_id, source_path, source_cwd, imported_at) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        )
        .bind(session_id)
        .bind(tool)
        .bind(native_id)
        .bind(source_path)
        .bind(source_cwd)
        .bind(now.to_rfc3339())
        .execute(self.db.pool())
        .await?;
        Ok(())
    }

    /// The origin of `session_id`, if this session was imported.
    pub async fn get(
        &self,
        session_id: &str,
    ) -> Result<Option<ForeignSessionOriginRecord>, StorageError> {
        let row = sqlx::query_as::<_, OriginRow>(
            "SELECT session_id, tool, native_id, source_path, source_cwd, imported_at \
             FROM foreign_session_origins WHERE session_id = ?1",
        )
        .bind(session_id)
        .fetch_optional(self.db.pool())
        .await?;
        Ok(row.map(Into::into))
    }
}

type OriginRow = (String, String, String, String, Option<String>, String);

impl From<OriginRow> for ForeignSessionOriginRecord {
    fn from(
        (session_id, tool, native_id, source_path, source_cwd, imported_at): OriginRow,
    ) -> Self {
        Self {
            session_id,
            tool,
            native_id,
            source_path,
            source_cwd,
            imported_at,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Database, SessionRecord, SessionRepository};
    use leveler_core::SessionId;

    async fn session(db: &Database, goal: &str) -> SessionId {
        let record = SessionRecord::new("/repo", goal, "mock/m", leveler_core::now());
        SessionRepository::new(db).create(&record).await.unwrap();
        SessionId::new(record.id)
    }

    #[tokio::test]
    async fn an_origin_round_trips_with_its_exact_source() {
        let db = Database::connect_in_memory().await.unwrap();
        let session = session(&db, "imported").await;
        let repo = ForeignSessionOriginRepository::new(&db);
        repo.insert(
            session.as_str(),
            "claude-code",
            "ff46f9d0-c283-49fe-b557-a4d92d8668db",
            "/home/u/.claude/projects/p/ff46f9d0-c283-49fe-b557-a4d92d8668db.jsonl",
            Some("/repo"),
            leveler_core::now(),
        )
        .await
        .unwrap();

        let origin = repo.get(session.as_str()).await.unwrap().expect("origin");
        assert_eq!(origin.session_id, session.as_str());
        assert_eq!(origin.tool, "claude-code");
        assert_eq!(origin.native_id, "ff46f9d0-c283-49fe-b557-a4d92d8668db");
        assert_eq!(origin.source_cwd.as_deref(), Some("/repo"));
        assert!(origin.imported_at.contains('T'), "{}", origin.imported_at);
        assert!(
            repo.get("missing").await.unwrap().is_none(),
            "an ordinary session has no origin"
        );
    }

    #[tokio::test]
    async fn deleting_the_session_deletes_its_origin() {
        let db = Database::connect_in_memory().await.unwrap();
        let session = session(&db, "imported").await;
        let repo = ForeignSessionOriginRepository::new(&db);
        repo.insert(
            session.as_str(),
            "codex",
            "01a1a979-1150-73b3-af45-275d61a036d0",
            "/home/u/.codex/sessions/2026/01/02/rollout-x.jsonl",
            None,
            leveler_core::now(),
        )
        .await
        .unwrap();

        SessionRepository::new(&db).delete(&session).await.unwrap();
        assert!(
            repo.get(session.as_str()).await.unwrap().is_none(),
            "the origin must not outlive the session it describes"
        );
    }
}
