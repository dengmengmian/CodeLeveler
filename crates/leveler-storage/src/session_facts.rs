//! Read-only task directory facts. Scanning never creates or migrates a store.
use crate::{Database, EventRecord, SessionRecord, StorageError, TurnRecord};
use std::path::Path;

/// Durable evidence needed by the shared client task projection.
#[derive(Debug, Clone)]
pub struct SessionFacts {
    /// Canonical session metadata, including optional primary workspace.
    pub session: SessionRecord,
    /// Archive is visibility metadata, separate from lifecycle.
    pub archived_at: Option<String>,
    /// Last durable activity across session, messages, turns and events.
    pub last_activity_at: String,
    /// First initiating user text for legacy placeholder titles.
    pub first_user_text: Option<String>,
    /// Turn boundaries and ownership, ordered by ordinal.
    pub turns: Vec<TurnRecord>,
    /// Latest task terminal, if any.
    pub terminal: Option<EventRecord>,
    /// Highest durable turn ordinal evidenced at the terminal's event sequence.
    /// A later admitted row is newer even before its first event is written.
    pub terminal_turn_ordinal: Option<i64>,
    /// Latest task/turn execution event sequence; earlier terminals are historical.
    pub latest_start_sequence: Option<i64>,
}

impl Database {
    /// Read all summary facts without changing the database or its schema.
    pub async fn session_facts(
        &self,
        include_archived: bool,
    ) -> Result<Vec<SessionFacts>, StorageError> {
        self.session_facts_filtered(include_archived, None).await
    }

    /// Read one session's projection evidence, including archived history,
    /// without scanning other sessions in the same store.
    pub async fn session_facts_for(
        &self,
        id: &leveler_core::SessionId,
    ) -> Result<Option<SessionFacts>, StorageError> {
        Ok(self
            .session_facts_filtered(true, Some(id.as_str()))
            .await?
            .into_iter()
            .next())
    }

    async fn session_facts_filtered(
        &self,
        include_archived: bool,
        id: Option<&str>,
    ) -> Result<Vec<SessionFacts>, StorageError> {
        // All evidence is read from one SQLite snapshot, even if a live writer
        // commits a new turn while the directory is being built.
        let mut tx = self.pool().begin().await?;
        let rows = sqlx::query_as::<_, crate::session_repo::SessionRow>(
            "SELECT id,repository,goal,status,model,state,created_at,updated_at,collaboration,work_profile FROM sessions WHERE (?1 OR archived_at IS NULL) AND (?2 IS NULL OR id=?2) ORDER BY updated_at DESC,id")
            .bind(include_archived).bind(id).fetch_all(&mut *tx).await?;
        let mut facts = Vec::with_capacity(rows.len());
        for row in rows {
            let session = row.decode()?;
            let archived_at: Option<String> =
                sqlx::query_scalar("SELECT archived_at FROM sessions WHERE id=?1")
                    .bind(&session.id)
                    .fetch_one(&mut *tx)
                    .await?;
            let turns = sqlx::query_as::<_, TurnRecord>("SELECT id,session_id,ordinal,kind,payload,status,created_at,finished_at,owner_boot_id FROM turns WHERE session_id=?1 ORDER BY ordinal")
                .bind(&session.id).fetch_all(&mut *tx).await?;
            let terminal = sqlx::query_as::<_, EventRecord>("SELECT id,session_id,turn_id,sequence,type AS event_type,payload,created_at,schema_version FROM events WHERE session_id=?1 AND type='task_finished' ORDER BY sequence DESC LIMIT 1")
                .bind(&session.id).fetch_optional(&mut *tx).await?;
            let terminal_turn_ordinal: Option<i64> = if let Some(terminal) = &terminal {
                sqlx::query_scalar("SELECT MAX(turns.ordinal) FROM turns JOIN events ON events.turn_id=turns.id WHERE turns.session_id=?1 AND events.session_id=?1 AND events.sequence<=?2")
                    .bind(&session.id).bind(terminal.sequence).fetch_one(&mut *tx).await?
            } else {
                None
            };
            let latest_start_sequence: Option<i64> = sqlx::query_scalar("SELECT MAX(sequence) FROM events WHERE session_id=?1 AND type IN ('task_started','turn_started','turn_finished')")
                .bind(&session.id).fetch_one(&mut *tx).await?;
            let last_activity_at: String = sqlx::query_scalar("SELECT MAX(ts) FROM (SELECT updated_at AS ts FROM sessions WHERE id=?1 UNION ALL SELECT created_at FROM session_messages WHERE session_id=?1 UNION ALL SELECT created_at FROM events WHERE session_id=?1 UNION ALL SELECT COALESCE(finished_at,created_at) FROM turns WHERE session_id=?1)")
                .bind(&session.id).fetch_one(&mut *tx).await?;
            let payload: Option<String> = sqlx::query_scalar("SELECT payload FROM session_messages WHERE session_id=?1 AND json_extract(payload,'$.role')='user' ORDER BY ordinal LIMIT 1")
                .bind(&session.id).fetch_optional(&mut *tx).await?;
            let first_user_text = payload
                .map(|text| {
                    serde_json::from_str::<serde_json::Value>(&text)
                        .map_err(|e| StorageError::InvalidData(e.to_string()))
                })
                .transpose()?
                .and_then(|v| {
                    v.get("content")
                        .and_then(|v| v.as_array())
                        .and_then(|parts| {
                            parts.iter().find_map(|part| {
                                (part.get("type")?.as_str()? == "text")
                                    .then(|| part.get("text")?.as_str().map(str::to_owned))
                                    .flatten()
                            })
                        })
                });
            facts.push(SessionFacts {
                session,
                archived_at,
                last_activity_at,
                first_user_text,
                turns,
                terminal,
                terminal_turn_ordinal,
                latest_start_sequence,
            });
        }
        tx.commit().await?;
        Ok(facts)
    }

    /// Open an existing database for read-only queries. Missing/old/corrupt
    /// sources are errors rather than an empty successful directory.
    pub async fn connect_read_only(path: &Path) -> Result<Self, StorageError> {
        let options = sqlx::sqlite::SqliteConnectOptions::new()
            .filename(path)
            .read_only(true)
            .busy_timeout(std::time::Duration::from_millis(500))
            .foreign_keys(true);
        let pool = sqlx::sqlite::SqlitePoolOptions::new()
            .max_connections(1)
            .connect_with(options)
            .await?;
        Ok(Self { pool })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::SessionRepository;
    #[tokio::test]
    async fn exclusive_locked_source_is_a_read_error_not_an_empty_directory() {
        use sqlx::Connection;
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("sessions.db");
        let db = Database::connect(&path).await.unwrap();
        let row = SessionRecord::new("/workspace", "task", "m", leveler_core::now());
        SessionRepository::new(&db).create(&row).await.unwrap();
        db.pool().close().await;
        let mut writer = sqlx::SqliteConnection::connect_with(
            &sqlx::sqlite::SqliteConnectOptions::new()
                .filename(&path)
                .journal_mode(sqlx::sqlite::SqliteJournalMode::Delete),
        )
        .await
        .unwrap();
        sqlx::query("BEGIN EXCLUSIVE")
            .execute(&mut writer)
            .await
            .unwrap();
        let result = async {
            Database::connect_read_only(&path)
                .await?
                .session_facts(false)
                .await
        }
        .await;
        sqlx::query("ROLLBACK").execute(&mut writer).await.unwrap();
        let Err(StorageError::Sqlx(error)) = result else {
            panic!("locked source must fail explicitly: {result:?}")
        };
        assert!(
            matches!(
                error
                    .as_database_error()
                    .and_then(|error| error.code())
                    .as_deref(),
                Some("5" | "6")
            ),
            "expected SQLite busy/locked error: {error}"
        );
    }
    #[tokio::test]
    async fn terminal_coverage_does_not_include_later_unannounced_turns() {
        let db = Database::connect_in_memory().await.unwrap();
        let row = SessionRecord::without_workspace("goal", "m", leveler_core::now());
        SessionRepository::new(&db).create(&row).await.unwrap();
        let id = leveler_core::SessionId::new(row.id);
        let turns = crate::TurnRepository::new(&db);
        let turn = turns
            .start(&id, "chat", None, leveler_core::now())
            .await
            .unwrap();
        let turn_id = leveler_core::TurnId::new(turn.id);
        let events = crate::EventRepository::new(&db);
        events
            .append(
                &id,
                Some(&turn_id),
                "turn_started",
                "{}",
                leveler_core::now(),
            )
            .await
            .unwrap();
        turns
            .finish(&turn_id, "completed", leveler_core::now())
            .await
            .unwrap();
        events
            .append(&id, None, "task_finished", "{}", leveler_core::now())
            .await
            .unwrap();
        let first = db.session_facts_for(&id).await.unwrap().unwrap();
        assert_eq!(first.terminal_turn_ordinal, Some(1));
        turns
            .start(&id, "chat", None, leveler_core::now())
            .await
            .unwrap();
        let later = db.session_facts_for(&id).await.unwrap().unwrap();
        assert_eq!(later.terminal_turn_ordinal, Some(1));
        assert_eq!(later.turns.last().unwrap().ordinal, 2);
    }
    #[tokio::test]
    async fn read_only_directory_keeps_archived_and_optional_workspace_facts() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("sessions.db");
        let db = Database::connect(&path).await.unwrap();
        let row = SessionRecord::new("/deleted/checkout", "task", "m", leveler_core::now());
        SessionRepository::new(&db).create(&row).await.unwrap();
        let id = leveler_core::SessionId::new(row.id.clone());
        SessionRepository::new(&db)
            .set_archived(&id, Some(leveler_core::now()))
            .await
            .unwrap();
        let facts = Database::connect_read_only(&path)
            .await
            .unwrap()
            .session_facts(true)
            .await
            .unwrap();
        assert_eq!(facts.len(), 1);
        assert_eq!(
            facts[0].session.repository.as_deref(),
            Some("/deleted/checkout")
        );
        assert!(facts[0].archived_at.is_some());
        assert_eq!(
            Database::connect_read_only(&path)
                .await
                .unwrap()
                .session_facts_for(&id)
                .await
                .unwrap()
                .unwrap()
                .session
                .id,
            row.id
        );
        assert!(
            Database::connect_read_only(&path)
                .await
                .unwrap()
                .session_facts_for(&leveler_core::SessionId::new("missing"))
                .await
                .unwrap()
                .is_none()
        );
        assert!(
            Database::connect_read_only(&path)
                .await
                .unwrap()
                .session_facts(false)
                .await
                .unwrap()
                .is_empty()
        );
        let missing = tmp.path().join("missing.db");
        assert!(Database::connect_read_only(&missing).await.is_err());
        assert!(!missing.exists());
    }
}
