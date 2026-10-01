//! Database connection and migration.

use std::path::Path;
use std::str::FromStr;

use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions};
use sqlx::{Pool, Sqlite};

/// Embedded migrations, applied at startup.
static MIGRATOR: sqlx::migrate::Migrator = sqlx::migrate!("./migrations");

/// Storage-layer errors.
#[derive(Debug, thiserror::Error)]
pub enum StorageError {
    /// The query itself failed — connection, constraint, or syntax.
    #[error("database error: {0}")]
    Sqlx(#[from] sqlx::Error),
    /// A migration could not be applied at startup.
    #[error("migration error: {0}")]
    Migrate(#[from] sqlx::migrate::MigrateError),
    /// A row was read but could not be decoded: an unknown enum string, an
    /// unparseable timestamp, or a payload version newer than this build
    /// understands. Signals corruption or a downgrade, never a transient fault,
    /// so retrying will not help.
    #[error("invalid persisted data: {0}")]
    InvalidData(String),
}

/// A handle to the SQLite database (WAL mode, foreign keys on).
#[derive(Debug, Clone)]
pub struct Database {
    pub(crate) pool: Pool<Sqlite>,
}

impl Database {
    /// Open (creating if needed) a database at `path` and run migrations.
    pub async fn connect(path: &Path) -> Result<Self, StorageError> {
        let options = SqliteConnectOptions::from_str(&format!("sqlite://{}", path.display()))
            .unwrap_or_else(|_| SqliteConnectOptions::new().filename(path))
            .create_if_missing(true)
            .journal_mode(SqliteJournalMode::Wal)
            .busy_timeout(std::time::Duration::from_secs(5))
            .foreign_keys(true);

        // SQLite cannot change FK enforcement inside SQLx's migration transaction.
        // Keep migrations on a private connection so rebuilding the parent table
        // cannot cascade-delete child history. Never expose this pool to callers.
        let migration_pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect_with(options.clone().foreign_keys(false))
            .await?;
        let result = async {
            MIGRATOR.run(&migration_pool).await?;
            if !sqlx::query("PRAGMA foreign_key_check")
                .fetch_all(&migration_pool)
                .await?
                .is_empty()
            {
                return Err(StorageError::InvalidData(
                    "migration left broken foreign keys".into(),
                ));
            }
            Ok::<_, StorageError>(())
        }
        .await;
        migration_pool.close().await;
        result?;
        let pool = SqlitePoolOptions::new()
            .max_connections(5)
            .connect_with(options)
            .await?;
        Ok(Self { pool })
    }

    /// Open an in-memory database (used by tests) and run migrations.
    pub async fn connect_in_memory() -> Result<Self, StorageError> {
        let options = SqliteConnectOptions::from_str("sqlite::memory:")
            .unwrap()
            .foreign_keys(false);
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect_with(options)
            .await?;
        MIGRATOR.run(&pool).await?;
        sqlx::query("PRAGMA foreign_keys=ON").execute(&pool).await?;
        Ok(Self { pool })
    }

    pub(crate) fn pool(&self) -> &Pool<Sqlite> {
        &self.pool
    }
}

/// Best-effort, read-only peek at the repository path recorded in a state
/// dir's `sessions.db` — the discovery fallback for state dirs that predate
/// the `.repository-root` ownership marker. Never creates or migrates the
/// database; any failure (missing file, foreign schema, lock) is `None`.
/// Reads the most recently updated row so a repository that moved reports its
/// latest location.
pub async fn peek_repository(db_path: &Path) -> Option<String> {
    if !db_path.is_file() {
        return None;
    }
    let options = SqliteConnectOptions::new()
        .filename(db_path)
        .read_only(true)
        .busy_timeout(std::time::Duration::from_millis(500));
    let pool = SqlitePoolOptions::new()
        .max_connections(1)
        .connect_with(options)
        .await
        .ok()?;
    let repository: Option<String> =
        sqlx::query_scalar("SELECT repository FROM sessions ORDER BY updated_at DESC LIMIT 1")
            .fetch_optional(&pool)
            .await
            .ok()?;
    pool.close().await;
    repository.filter(|r| !r.trim().is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn optional_workspace_migration_preserves_old_children_and_admission() {
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect_with(
                SqliteConnectOptions::new()
                    .in_memory(true)
                    .foreign_keys(false),
            )
            .await
            .unwrap();
        let old = sqlx::migrate::Migrator {
            migrations: std::borrow::Cow::Owned(
                MIGRATOR
                    .iter()
                    .filter(|m| m.version < 37)
                    .cloned()
                    .collect(),
            ),
            ..sqlx::migrate::Migrator::DEFAULT
        };
        old.run(&pool).await.unwrap();
        sqlx::query("INSERT INTO sessions(id,repository,goal,status,model,state,created_at,updated_at) VALUES('old','/old','goal','created','m','understand','t','t')").execute(&pool).await.unwrap();
        sqlx::query(
            "INSERT INTO turns(id,session_id,ordinal,created_at) VALUES('child','old',1,'t')",
        )
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query("INSERT INTO session_messages(session_id,ordinal,payload,created_at) VALUES('old',1,'{}','t')").execute(&pool).await.unwrap();
        sqlx::query("INSERT INTO events(id,session_id,turn_id,sequence,type,payload,created_at) VALUES('history','old','child',1,'test','{}','t')").execute(&pool).await.unwrap();
        let terminal_payload = r#"{"type":"task_finished","payload":{"outcome":"completed","reason":null,"stop":"completed","warnings":[]}}"#;
        sqlx::query("INSERT INTO events(id,session_id,turn_id,sequence,type,payload,created_at) VALUES('terminal','old',NULL,2,'task_finished',?1,'t')").bind(terminal_payload).execute(&pool).await.unwrap();
        sqlx::query("UPDATE sessions SET outcome='completed',status='completed',state='complete' WHERE id='old'").execute(&pool).await.unwrap();
        sqlx::query("INSERT INTO command_receipts(command_id,session_id,command_fingerprint,issued_at,admitted_at,status,admitted_by_boot) VALUES('dedup','old','fingerprint','issued','admitted','completed','boot')").execute(&pool).await.unwrap();
        MIGRATOR.run(&pool).await.unwrap();
        let terminal: (String, String, Option<String>, i64, String) = sqlx::query_as(
            "SELECT session_id,type,turn_id,sequence,payload FROM events WHERE id='terminal'",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(
            terminal,
            (
                "old".into(),
                "task_finished".into(),
                None,
                2,
                terminal_payload.into()
            )
        );
        let receipt:(String,String,String,String,String,Option<String>)=sqlx::query_as("SELECT session_id,command_fingerprint,issued_at,admitted_at,status,admitted_by_boot FROM command_receipts WHERE command_id='dedup'").fetch_one(&pool).await.unwrap();
        assert_eq!(
            receipt,
            (
                "old".into(),
                "fingerprint".into(),
                "issued".into(),
                "admitted".into(),
                "completed".into(),
                Some("boot".into())
            )
        );
        assert!(sqlx::query("INSERT INTO command_receipts(command_id,session_id,command_fingerprint,issued_at,admitted_at) VALUES('dedup','old','fingerprint','t','t')").execute(&pool).await.is_err(),"dedup identity stays unique after rebuild");
        let lifecycle: (String, String, String) =
            sqlx::query_as("SELECT status,state,outcome FROM sessions WHERE id='old'")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(
            lifecycle,
            ("completed".into(), "complete".into(), "completed".into())
        );
        assert_eq!(
            sqlx::query_scalar::<_, i64>(
                "SELECT COUNT(*) FROM session_messages WHERE session_id='old'"
            )
            .fetch_one(&pool)
            .await
            .unwrap(),
            1
        );
        assert_eq!(
            sqlx::query_scalar::<_, i64>(
                "SELECT COUNT(*) FROM events WHERE id='history' AND turn_id='child'"
            )
            .fetch_one(&pool)
            .await
            .unwrap(),
            1
        );
        let old_workspace: Option<String> =
            sqlx::query_scalar("SELECT repository FROM sessions WHERE id='old'")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(old_workspace.as_deref(), Some("/old"));
        sqlx::query("INSERT INTO sessions(id,repository,goal,status,model,state,created_at,updated_at) VALUES('none',NULL,'goal','created','m','understand','t','t')").execute(&pool).await.unwrap();
        assert_eq!(
            sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM turns WHERE id='child'")
                .fetch_one(&pool)
                .await
                .unwrap(),
            1
        );
        assert!(
            sqlx::query("PRAGMA foreign_key_check")
                .fetch_all(&pool)
                .await
                .unwrap()
                .is_empty()
        );
        sqlx::query("PRAGMA foreign_keys=ON")
            .execute(&pool)
            .await
            .unwrap();
        assert!(
            sqlx::query(
                "INSERT INTO turns(id,session_id,ordinal,created_at) VALUES('bad','missing',1,'t')"
            )
            .execute(&pool)
            .await
            .is_err()
        );
        sqlx::query("INSERT INTO turns(id,session_id,ordinal,kind,payload,created_at) VALUES('fresh','none',1,'user','{\"initiating_message\":{}}','t')").execute(&pool).await.unwrap();
        assert_eq!(
            sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM memory_inbox WHERE turn_id='fresh'")
                .fetch_one(&pool)
                .await
                .unwrap(),
            1
        );
        sqlx::query("DELETE FROM sessions WHERE id='none'")
            .execute(&pool)
            .await
            .unwrap();
        assert_eq!(
            sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM turns WHERE id='fresh'")
                .fetch_one(&pool)
                .await
                .unwrap(),
            0
        );
        assert_eq!(
            sqlx::query_scalar::<_, i64>(
                "SELECT COUNT(*) FROM sqlite_master WHERE type='index' AND name='idx_turns_session'"
            )
            .fetch_one(&pool)
            .await
            .unwrap(),
            1
        );
    }

    #[tokio::test]
    async fn migrations_apply_on_in_memory_db() {
        let db = Database::connect_in_memory().await.unwrap();
        // The sessions table should exist and be queryable.
        let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM sessions")
            .fetch_one(db.pool())
            .await
            .unwrap();
        assert_eq!(count, 0);
    }

    #[tokio::test]
    async fn reconnecting_a_file_db_keeps_data_and_remigrates_idempotently() {
        let dir = std::env::temp_dir().join(format!(
            "leveler-db-remigrate-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("sessions.db");

        {
            let db = Database::connect(&path).await.unwrap();
            sqlx::query(
                "INSERT INTO sessions (id, repository, goal, status, model, state, \
                 created_at, updated_at) VALUES ('s1','/r','g','created','m','understand','t','t')",
            )
            .execute(db.pool())
            .await
            .unwrap();
        }

        // Reopen: migrations re-run (no-ops), data and new-column defaults intact.
        let db = Database::connect(&path).await.unwrap();
        let (goal, kind): (String, String) =
            sqlx::query_as("SELECT goal, kind FROM sessions WHERE id = 's1'")
                .fetch_one(db.pool())
                .await
                .unwrap();
        assert_eq!(goal, "g");
        assert_eq!(kind, "direct");

        drop(db);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn migration_0016_backfills_tasks_for_legacy_sessions() {
        use sqlx::ConnectOptions;
        use sqlx::migrate::Migrate;

        let dir = std::env::temp_dir().join(format!(
            "leveler-db-task-backfill-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("sessions.db");

        // Build a genuine legacy database: apply every migration BEFORE the
        // tasks table (recorded in _sqlx_migrations exactly as an old binary
        // would have), then write session rows.
        {
            let mut conn = SqliteConnectOptions::new()
                .filename(&path)
                .create_if_missing(true)
                .connect()
                .await
                .unwrap();
            conn.ensure_migrations_table().await.unwrap();
            for migration in MIGRATOR.migrations.iter().filter(|m| m.version < 16) {
                conn.apply(migration).await.unwrap();
            }
            sqlx::query(
                "INSERT INTO sessions (id, repository, goal, status, model, state, \
                 created_at, updated_at) VALUES \
                 ('legacy-a','/r','g','completed','m','complete','t1','t1'), \
                 ('legacy-b','/r','g','created','m','understand','t2','t2')",
            )
            .execute(&mut conn)
            .await
            .unwrap();
        }

        // Reopening runs the remaining migrations; 0016 must backfill one
        // task per legacy session, deterministically (task id = session id).
        let db = Database::connect(&path).await.unwrap();
        let rows: Vec<(String, String, String)> =
            sqlx::query_as("SELECT id, session_id, created_at FROM tasks ORDER BY id")
                .fetch_all(db.pool())
                .await
                .unwrap();
        assert_eq!(
            rows,
            vec![
                ("legacy-a".into(), "legacy-a".into(), "t1".into()),
                ("legacy-b".into(), "legacy-b".into(), "t2".into()),
            ]
        );
        // Legacy sessions survive untouched (resume/list depend on them).
        let sessions: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM sessions")
            .fetch_one(db.pool())
            .await
            .unwrap();
        assert_eq!(sessions, 2);

        drop(db);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn migration_0017_leaves_legacy_tasks_unowned() {
        use sqlx::ConnectOptions;
        use sqlx::migrate::Migrate;

        let dir = std::env::temp_dir().join(format!(
            "leveler-db-ownership-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("sessions.db");

        // A genuine pre-0017 database: sessions + backfilled tasks exist.
        {
            let mut conn = SqliteConnectOptions::new()
                .filename(&path)
                .create_if_missing(true)
                .connect()
                .await
                .unwrap();
            conn.ensure_migrations_table().await.unwrap();
            for migration in MIGRATOR.migrations.iter().filter(|m| m.version < 17) {
                conn.apply(migration).await.unwrap();
            }
            sqlx::query(
                "INSERT INTO sessions (id, repository, goal, status, model, state, \
                 created_at, updated_at) VALUES ('legacy','/r','g','completed','m','complete','t','t')",
            )
            .execute(&mut conn)
            .await
            .unwrap();
            sqlx::query(
                "INSERT INTO tasks (id, session_id, created_at) VALUES ('legacy','legacy','t')",
            )
            .execute(&mut conn)
            .await
            .unwrap();
        }

        // Reopening applies 0017: historical tasks stay UNOWNED — the
        // migration must not invent a runtime authority it cannot know.
        let db = Database::connect(&path).await.unwrap();
        let (runtime, epoch): (Option<String>, i64) =
            sqlx::query_as("SELECT owner_runtime_id, owner_epoch FROM tasks WHERE id = 'legacy'")
                .fetch_one(db.pool())
                .await
                .unwrap();
        assert_eq!(runtime, None);
        assert_eq!(epoch, 0);

        drop(db);
        std::fs::remove_dir_all(&dir).ok();
    }

    /// Rows written before boots were recorded keep a NULL boot: no boot can
    /// be proven dead for them, and the migration must not invent one.
    #[tokio::test]
    async fn migration_0025_leaves_existing_rows_without_a_boot() {
        use sqlx::ConnectOptions;
        use sqlx::migrate::Migrate;

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sessions.db");
        {
            let mut conn = SqliteConnectOptions::new()
                .filename(&path)
                .create_if_missing(true)
                .connect()
                .await
                .unwrap();
            conn.ensure_migrations_table().await.unwrap();
            for migration in MIGRATOR.migrations.iter().filter(|m| m.version < 25) {
                conn.apply(migration).await.unwrap();
            }
            for statement in [
                "INSERT INTO sessions (id, repository, goal, status, model, state, \
                 created_at, updated_at) VALUES ('s','/r','g','running','m','understand','t','t')",
                "INSERT INTO tasks (id, session_id, created_at, owner_runtime_id, owner_epoch) \
                 VALUES ('s','s','t','rt',3)",
                "INSERT INTO turns (id, session_id, ordinal, kind, status, created_at) \
                 VALUES ('done','s',1,'user','completed','t')",
                "INSERT INTO turns (id, session_id, ordinal, kind, status, created_at) \
                 VALUES ('live','s',2,'user','running','t')",
            ] {
                sqlx::query(statement).execute(&mut conn).await.unwrap();
            }
        }

        let db = Database::connect(&path).await.unwrap();
        let task: (Option<String>, Option<String>, i64) = sqlx::query_as(
            "SELECT owner_runtime_id, owner_boot_id, owner_epoch FROM tasks WHERE id = 's'",
        )
        .fetch_one(db.pool())
        .await
        .unwrap();
        assert_eq!(task, (Some("rt".to_string()), None, 3));
        let turns: Vec<(String, String, Option<String>)> =
            sqlx::query_as("SELECT id, status, owner_boot_id FROM turns ORDER BY ordinal")
                .fetch_all(db.pool())
                .await
                .unwrap();
        assert_eq!(
            turns,
            vec![
                ("done".to_string(), "completed".to_string(), None),
                ("live".to_string(), "running".to_string(), None),
            ]
        );
    }

    /// Creating the inbox must not reinterpret historical turns as newly
    /// accepted work. Only inserts made after the admission trigger exists
    /// belong in the queue.
    #[tokio::test]
    async fn migration_0027_does_not_backfill_historical_turns() {
        use sqlx::ConnectOptions;
        use sqlx::migrate::Migrate;

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sessions.db");
        {
            let mut conn = SqliteConnectOptions::new()
                .filename(&path)
                .create_if_missing(true)
                .connect()
                .await
                .unwrap();
            conn.ensure_migrations_table().await.unwrap();
            for migration in MIGRATOR.migrations.iter().filter(|m| m.version < 27) {
                conn.apply(migration).await.unwrap();
            }
            sqlx::query(
                "INSERT INTO sessions (id, repository, goal, status, model, state, \
                 created_at, updated_at) VALUES \
                 ('s','/r','g','completed','provider/model','complete','t','t')",
            )
            .execute(&mut conn)
            .await
            .unwrap();
            sqlx::query(
                "INSERT INTO turns (id, session_id, ordinal, kind, payload, status, created_at) \
                 VALUES ('historical','s',1,'user','{}','completed','t')",
            )
            .execute(&mut conn)
            .await
            .unwrap();
        }

        let db = Database::connect(&path).await.unwrap();
        let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM memory_inbox")
            .fetch_one(db.pool())
            .await
            .unwrap();
        assert_eq!(count, 0);
    }

    #[tokio::test]
    async fn migration_0036_maps_legacy_profiles_and_preserves_explicit_memory_policy() {
        use sqlx::ConnectOptions;
        use sqlx::migrate::Migrate;

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sessions.db");
        {
            let mut conn = SqliteConnectOptions::new()
                .filename(&path)
                .create_if_missing(true)
                .connect()
                .await
                .unwrap();
            conn.ensure_migrations_table().await.unwrap();
            for migration in MIGRATOR.migrations.iter().filter(|m| m.version < 36) {
                conn.apply(migration).await.unwrap();
            }
            for (profile, memory_enabled) in [("economy", 1), ("core", 0), ("balanced", 1)] {
                sqlx::query(
                    "INSERT INTO sessions (id, repository, goal, status, model, state, \
                     created_at, updated_at, work_profile, memory_enabled) \
                     VALUES (?1, '/r', 'g', 'created', 'm', 'understand', 't', 't', ?1, ?2)",
                )
                .bind(profile)
                .bind(memory_enabled)
                .execute(&mut conn)
                .await
                .unwrap();
            }
        }
        let db = Database::connect(&path).await.unwrap();
        let profiles: Vec<(String, String, i64)> =
            sqlx::query_as("SELECT id, work_profile, memory_enabled FROM sessions ORDER BY id")
                .fetch_all(db.pool())
                .await
                .unwrap();
        assert_eq!(
            profiles,
            vec![
                ("balanced".into(), "single".into(), 1),
                ("core".into(), "single".into(), 0),
                ("economy".into(), "single".into(), 1),
            ]
        );
        assert_eq!(
            sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM goal_capabilities")
                .fetch_one(db.pool())
                .await
                .unwrap(),
            0
        );
        let trigger: String = sqlx::query_scalar(
            "SELECT sql FROM sqlite_master WHERE type = 'trigger' AND name = 'admit_fresh_turn_to_memory_inbox'",
        ).fetch_one(db.pool()).await.unwrap();
        assert!(!trigger.contains("work_profile"));
        assert!(trigger.contains("memory_enabled"));
    }

    #[tokio::test]
    async fn peek_repository_reads_latest_row_without_migrating() {
        let dir = std::env::temp_dir().join(format!(
            "leveler-db-peek-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("sessions.db");
        {
            let db = Database::connect(&path).await.unwrap();
            sqlx::query(
                "INSERT INTO sessions (id, repository, goal, status, model, state, \
                 created_at, updated_at) VALUES \
                 ('s1','/old/location','g','created','m','understand','t1','t1'), \
                 ('s2','/new/location','g','created','m','understand','t2','t2')",
            )
            .execute(db.pool())
            .await
            .unwrap();
        }

        // Most recently updated row wins (a moved repository reports its
        // latest location).
        assert_eq!(
            peek_repository(&path).await.as_deref(),
            Some("/new/location")
        );
        // Missing file and non-database files are None, not errors.
        assert_eq!(peek_repository(&dir.join("absent.db")).await, None);
        let junk = dir.join("junk.db");
        std::fs::write(&junk, b"not a database").unwrap();
        assert_eq!(peek_repository(&junk).await, None);

        std::fs::remove_dir_all(&dir).ok();
    }
}
