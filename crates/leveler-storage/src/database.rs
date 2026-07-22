//! Database connection and migration.

use std::path::Path;
use std::str::FromStr;

use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions};
use sqlx::{Pool, Sqlite};

/// Embedded migrations, applied at startup.
static MIGRATOR: sqlx::migrate::Migrator = sqlx::migrate!("../../migrations");

/// Storage-layer errors.
#[derive(Debug, thiserror::Error)]
pub enum StorageError {
    #[error("database error: {0}")]
    Sqlx(#[from] sqlx::Error),
    #[error("migration error: {0}")]
    Migrate(#[from] sqlx::migrate::MigrateError),
    #[error("invalid persisted data: {0}")]
    InvalidData(String),
}

/// A handle to the SQLite database (WAL mode, foreign keys on).
#[derive(Debug, Clone)]
pub struct Database {
    pool: Pool<Sqlite>,
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

        let pool = SqlitePoolOptions::new()
            .max_connections(5)
            .connect_with(options)
            .await?;

        MIGRATOR.run(&pool).await?;
        Ok(Self { pool })
    }

    /// Open an in-memory database (used by tests) and run migrations.
    pub async fn connect_in_memory() -> Result<Self, StorageError> {
        let options = SqliteConnectOptions::from_str("sqlite::memory:")
            .unwrap()
            .foreign_keys(true);
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect_with(options)
            .await?;
        MIGRATOR.run(&pool).await?;
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
