//! Read-only task directory facts. Scanning never creates or migrates a store.
use crate::{Database, EventRecord, SessionRecord, StorageError, TurnRecord};
use sqlx::SqliteConnection;
use std::collections::{HashMap, HashSet};
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
    /// Latest turn and every running turn, without input bodies, ordered by ordinal.
    pub turns: Vec<TurnRecord>,
    /// Latest task terminal, if any.
    pub terminal: Option<EventRecord>,
    /// Highest durable turn ordinal evidenced at the terminal's event sequence.
    /// A later admitted row is newer even before its first event is written.
    pub terminal_turn_ordinal: Option<i64>,
    /// Latest task/turn execution event sequence; earlier terminals are historical.
    pub latest_start_sequence: Option<i64>,
}

/// What a discovered source actually contains. Discovery never assumes this
/// binary's schema: a store is whatever the last writer left behind.
struct FactsSchema {
    sessions: Columns,
    turns: Option<Columns>,
    events: Option<Columns>,
    messages: Option<Columns>,
}

impl FactsSchema {
    async fn detect(conn: &mut SqliteConnection) -> Result<Self, StorageError> {
        let sessions = table_columns(conn, "sessions").await?;
        if !sessions.has("id") {
            return Err(StorageError::InvalidData(
                "store has no sessions table".into(),
            ));
        }
        Ok(Self {
            sessions,
            turns: optional_table(table_columns(conn, "turns").await?),
            events: optional_table(table_columns(conn, "events").await?),
            messages: optional_table(table_columns(conn, "session_messages").await?),
        })
    }
}

fn optional_table(columns: Columns) -> Option<Columns> {
    (!columns.0.is_empty()).then_some(columns)
}

#[derive(Default)]
struct Columns(HashSet<String>);

impl Columns {
    fn has(&self, name: &str) -> bool {
        self.0.contains(name)
    }

    /// The real column, or the value the migration that introduced it assigns
    /// to rows that predate it. Callers alias the result.
    fn project(&self, name: &str, fallback: &str) -> String {
        if self.has(name) {
            name.to_string()
        } else {
            fallback.to_string()
        }
    }
}

async fn table_columns(conn: &mut SqliteConnection, table: &str) -> Result<Columns, StorageError> {
    let rows: Vec<(String,)> = sqlx::query_as("SELECT name FROM pragma_table_info(?1)")
        .bind(table)
        .fetch_all(conn)
        .await?;
    Ok(Columns(rows.into_iter().map(|(name,)| name).collect()))
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
        // A source is whatever an older binary left behind. Probe what it
        // actually has instead of assuming this binary's schema: discovery is
        // read-only and must never migrate a store it merely lists.
        let schema = FactsSchema::detect(&mut tx).await?;
        // 0006 added these axes with exactly these defaults, so a pre-0006
        // store projects the same values its migration would have written.
        let collaboration = schema.sessions.project("collaboration", "'goal'");
        let work_profile = schema.sessions.project("work_profile", "'balanced'");
        let visibility = if schema.sessions.has("archived_at") {
            "(?1 OR archived_at IS NULL) AND (?2 IS NULL OR id=?2)"
        } else {
            "(?1 OR NOT ?1) AND (?2 IS NULL OR id=?2)"
        };
        let selected = format!("SELECT id FROM sessions WHERE {visibility}");
        let rows = sqlx::query_as::<_, crate::session_repo::SessionRow>(&format!(
            "SELECT id,repository,goal,status,model,state,created_at,updated_at,{collaboration} AS collaboration,{work_profile} AS work_profile FROM sessions WHERE {visibility} ORDER BY updated_at DESC,id"))
            .bind(include_archived).bind(id).fetch_all(&mut *tx).await?;
        // A fixed set of batched queries, independent of the session count.
        let mut archived: HashMap<String, Option<String>> = if schema.sessions.has("archived_at") {
            sqlx::query_as::<_, (String, Option<String>)>(&format!(
                "SELECT id,archived_at FROM sessions WHERE {visibility}"
            ))
            .bind(include_archived)
            .bind(id)
            .fetch_all(&mut *tx)
            .await?
            .into_iter()
            .collect()
        } else {
            HashMap::new()
        };
        let mut turns_by_session: HashMap<String, Vec<TurnRecord>> = HashMap::new();
        if let Some(columns) = &schema.turns {
            let kind = columns.project("kind", "'user'");
            let status = columns.project("status", "'running'");
            let finished_at = columns.project("finished_at", "NULL");
            let owner_boot_id = columns.project("owner_boot_id", "NULL");
            let turns = sqlx::query_as::<_, TurnRecord>(&format!(
                "SELECT id,session_id,ordinal,{kind} AS kind,NULL AS payload,{status} AS status,created_at,{finished_at} AS finished_at,{owner_boot_id} AS owner_boot_id FROM turns t WHERE session_id IN ({selected}) AND ({status}='running' OR ordinal=(SELECT MAX(ordinal) FROM turns newest WHERE newest.session_id=t.session_id)) ORDER BY session_id,ordinal"))
                .bind(include_archived).bind(id).fetch_all(&mut *tx).await?;
            for turn in turns {
                turns_by_session
                    .entry(turn.session_id.clone())
                    .or_default()
                    .push(turn);
            }
        }
        let mut terminals: HashMap<String, EventRecord> = HashMap::new();
        let mut coverage: HashMap<String, Option<i64>> = HashMap::new();
        let mut starts: HashMap<String, Option<i64>> = HashMap::new();
        if let Some(columns) = &schema.events {
            let version = columns.project("schema_version", "1");
            let records = sqlx::query_as::<_, EventRecord>(&format!(
                "SELECT id,session_id,turn_id,sequence,type AS event_type,payload,created_at,{version} AS schema_version FROM events e WHERE session_id IN ({selected}) AND type='task_finished' AND sequence=(SELECT MAX(sequence) FROM events terminal WHERE terminal.session_id=e.session_id AND terminal.type='task_finished')"))
                .bind(include_archived).bind(id).fetch_all(&mut *tx).await?;
            for record in records {
                terminals.insert(record.session_id.clone(), record);
            }
            starts = sqlx::query_as::<_, (String, Option<i64>)>(&format!(
                "SELECT session_id,MAX(sequence) FROM events WHERE session_id IN ({selected}) AND type IN ('task_started','turn_started','turn_finished') GROUP BY session_id"))
                .bind(include_archived).bind(id).fetch_all(&mut *tx).await?.into_iter().collect();
            if schema.turns.is_some() {
                coverage = sqlx::query_as::<_, (String, Option<i64>)>(&format!(
                    "SELECT t.session_id,MAX(t.ordinal) FROM turns t JOIN events e ON e.turn_id=t.id AND e.session_id=t.session_id WHERE t.session_id IN ({selected}) AND e.sequence<=(SELECT MAX(sequence) FROM events terminal WHERE terminal.session_id=t.session_id AND terminal.type='task_finished') GROUP BY t.session_id"))
                    .bind(include_archived).bind(id).fetch_all(&mut *tx).await?.into_iter().collect();
            }
        }
        let mut activity = vec![format!(
            "SELECT id AS session_id,updated_at AS ts FROM sessions WHERE {visibility}"
        )];
        if schema.messages.is_some() {
            activity.push(format!("SELECT session_id,MAX(created_at) AS ts FROM session_messages WHERE session_id IN ({selected}) GROUP BY session_id"));
        }
        if schema.events.is_some() {
            activity.push(format!("SELECT session_id,MAX(created_at) AS ts FROM events WHERE session_id IN ({selected}) GROUP BY session_id"));
        }
        if let Some(columns) = &schema.turns {
            let finished = if columns.has("finished_at") {
                "COALESCE(finished_at,created_at)"
            } else {
                "created_at"
            };
            activity.push(format!("SELECT session_id,MAX({finished}) AS ts FROM turns WHERE session_id IN ({selected}) GROUP BY session_id"));
        }
        let mut activity: HashMap<String, String> =
            sqlx::query_as::<_, (String, String)>(&format!(
                "SELECT session_id,MAX(ts) FROM ({}) GROUP BY session_id",
                activity.join(" UNION ALL ")
            ))
            .bind(include_archived)
            .bind(id)
            .fetch_all(&mut *tx)
            .await?
            .into_iter()
            .collect();
        let mut inputs: HashMap<String, String> = if schema.messages.is_some() {
            sqlx::query_as::<_, (String, String)>(&format!(
                "SELECT session_id,payload FROM session_messages m WHERE session_id IN ({selected}) AND ordinal=(SELECT MIN(ordinal) FROM session_messages first WHERE first.session_id=m.session_id AND json_extract(first.payload,'$.role')='user')"))
                .bind(include_archived).bind(id).fetch_all(&mut *tx).await?.into_iter().collect()
        } else {
            HashMap::new()
        };
        let mut facts = Vec::with_capacity(rows.len());
        for row in rows {
            let session = row.decode()?;
            let archived_at = archived.remove(&session.id).flatten();
            let turns = turns_by_session.remove(&session.id).unwrap_or_default();
            let terminal = terminals.remove(&session.id);
            let terminal_turn_ordinal = coverage.remove(&session.id).flatten();
            let latest_start_sequence = starts.remove(&session.id).flatten();
            let last_activity_at = activity.remove(&session.id).ok_or_else(|| {
                StorageError::InvalidData("session activity missing from read snapshot".into())
            })?;
            let payload = inputs.remove(&session.id);
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
    async fn directory_reads_only_lifecycle_turn_evidence() {
        let db = Database::connect_in_memory().await.unwrap();
        let record = SessionRecord::without_workspace("goal", "m", leveler_core::now());
        SessionRepository::new(&db).create(&record).await.unwrap();
        let id = leveler_core::SessionId::new(record.id);
        let turns = crate::TurnRepository::new(&db);
        for _ in 0..3 {
            let turn = turns
                .start(
                    &id,
                    "chat",
                    Some(r#"{"input":"large accepted input"}"#),
                    leveler_core::now(),
                )
                .await
                .unwrap();
            turns
                .finish(
                    &leveler_core::TurnId::new(turn.id),
                    "completed",
                    leveler_core::now(),
                )
                .await
                .unwrap();
        }
        let facts = db.session_facts_for(&id).await.unwrap().unwrap();
        assert_eq!(
            facts.turns.len(),
            1,
            "only the latest settled turn affects the directory"
        );
        assert!(
            facts.turns[0].payload.is_none(),
            "directory never needs the accepted input body"
        );
    }
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
    async fn read_only_directory_reads_a_pre_owner_boot_store_without_migrating_it() {
        use sqlx::Connection;

        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("sessions.db");
        {
            let mut conn = crate::database::create_legacy_database(&path, 25).await;
            for statement in [
                "INSERT INTO sessions (id, repository, goal, status, model, state, created_at, updated_at, collaboration, work_profile, archived_at) \
                 VALUES ('legacy','/old/checkout','g','completed','m','complete','2025-01-01T00:00:00Z','2025-01-02T00:00:00Z','goal','balanced','2025-01-03T00:00:00Z')",
                "INSERT INTO session_messages (session_id, ordinal, payload, created_at) \
                 VALUES ('legacy', 0, '{\"role\":\"user\",\"content\":[{\"type\":\"text\",\"text\":\"legacy goal\"}]}', '2025-01-01T00:00:00Z')",
                "INSERT INTO turns (id, session_id, ordinal, kind, payload, status, created_at, finished_at) \
                 VALUES ('t1','legacy',1,'user',NULL,'completed','2025-01-01T00:00:00Z','2025-01-01T00:01:00Z')",
                "INSERT INTO turns (id, session_id, ordinal, kind, payload, status, created_at, finished_at) \
                 VALUES ('t2','legacy',2,'user',NULL,'running','2025-01-02T00:00:00Z',NULL)",
                "INSERT INTO events (id, session_id, turn_id, sequence, type, payload, created_at, schema_version) \
                 VALUES ('e1','legacy','t1',1,'task_finished','{}','2025-01-01T00:01:00Z',3)",
            ] {
                sqlx::query(statement).execute(&mut conn).await.unwrap();
            }
            conn.close().await.unwrap();
        }

        let db = Database::connect_read_only(&path).await.unwrap();
        let facts = db.session_facts(true).await.unwrap();
        assert_eq!(facts.len(), 1);
        assert_eq!(facts[0].session.id, "legacy");
        assert_eq!(
            facts[0].session.repository.as_deref(),
            Some("/old/checkout")
        );
        assert_eq!(
            facts[0].archived_at.as_deref(),
            Some("2025-01-03T00:00:00Z")
        );
        assert_eq!(facts[0].first_user_text.as_deref(), Some("legacy goal"));
        assert_eq!(facts[0].turns.len(), 1);
        assert_eq!(facts[0].turns[0].status, "running");
        assert_eq!(facts[0].turns[0].owner_boot_id, None);
        assert_eq!(facts[0].terminal.as_ref().unwrap().sequence, 1);
        assert_eq!(facts[0].terminal_turn_ordinal, Some(1));

        // The read path is a projection, not an upgrade: the old store keeps
        // its schema and can still be migrated by the next runtime that owns it.
        let version: i64 = sqlx::query_scalar("SELECT MAX(version) FROM _sqlx_migrations")
            .fetch_one(db.pool())
            .await
            .unwrap();
        assert_eq!(version, 24);
        let columns: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM pragma_table_info('turns') WHERE name='owner_boot_id'",
        )
        .fetch_one(db.pool())
        .await
        .unwrap();
        assert_eq!(columns, 0);
    }

    #[tokio::test]
    async fn read_only_directory_reads_a_pre_session_axes_store_with_documented_defaults() {
        use sqlx::Connection;

        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("sessions.db");
        {
            let mut conn = crate::database::create_legacy_database(&path, 4).await;
            for statement in [
                "INSERT INTO sessions (id, repository, goal, status, model, state, created_at, updated_at) \
                 VALUES ('older','/old','g','completed','m','complete','2024-01-01T00:00:00Z','2024-01-03T00:00:00Z')",
                "INSERT INTO turns (id, session_id, ordinal, kind, payload, status, created_at, finished_at) \
                 VALUES ('t1','older',1,'user',NULL,'completed','2024-01-01T00:00:00Z','2024-01-01T00:01:00Z')",
                "INSERT INTO events (id, session_id, turn_id, sequence, type, payload, created_at) \
                 VALUES ('e1','older','t1',1,'task_finished','{}','2024-01-01T00:01:00Z')",
            ] {
                sqlx::query(statement).execute(&mut conn).await.unwrap();
            }
            conn.close().await.unwrap();
        }

        let facts = Database::connect_read_only(&path)
            .await
            .unwrap()
            .session_facts(true)
            .await
            .unwrap();
        assert_eq!(facts.len(), 1);
        // 0006 introduced these with exactly these legacy defaults.
        assert_eq!(facts[0].session.collaboration, "goal");
        assert_eq!(facts[0].session.work_profile, "balanced");
        assert_eq!(facts[0].archived_at, None);
        assert_eq!(facts[0].first_user_text, None);
        // 0004 introduced schema_version with default 1 for existing rows.
        assert_eq!(facts[0].terminal.as_ref().unwrap().schema_version, 1);
        assert_eq!(facts[0].turns[0].owner_boot_id, None);
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
