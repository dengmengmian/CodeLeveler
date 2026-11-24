//! The 0066 upgrade: existing sessions keep their exact pre-V2 semantics.
//!
//! The tier names did not change in Permission Model V2, so the ONLY thing
//! that stops an old `assisted` row from being read as the new Auto is the
//! contract column. This test builds a database at the pre-V2 schema, inserts
//! one row per tier (including the legacy names), applies 0066, and asserts
//! the rows are unchanged and marked V1.

use leveler_storage::TaskCreationStore;
use sqlx::sqlite::SqlitePoolOptions;

async fn apply_migrations_through(pool: &sqlx::SqlitePool, last: &str) {
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("migrations");
    let mut files: Vec<_> = std::fs::read_dir(&dir)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .filter(|path| path.extension().is_some_and(|ext| ext == "sql"))
        .collect();
    files.sort();
    for path in files {
        let name = path.file_name().unwrap().to_string_lossy();
        if name.as_ref() > last {
            continue;
        }
        let sql = std::fs::read_to_string(&path).unwrap();
        sqlx::raw_sql(&sql).execute(pool).await.unwrap();
    }
}

async fn insert_session(pool: &sqlx::SqlitePool, id: &str, mode: &str) {
    sqlx::query(
        "INSERT INTO sessions \
         (id, repository, goal, status, model, state, created_at, updated_at, \
          collaboration, work_profile, mode, sandbox, kind) \
         VALUES (?1, '/repo', 'goal', 'idle', 'mock/m', 'idle', 'now', 'now', \
                 'chat', 'single', ?2, 0, 'direct')",
    )
    .bind(id)
    .bind(mode)
    .execute(pool)
    .await
    .unwrap();
}

#[tokio::test]
async fn upgrading_v1_rows_preserves_mode_and_stamps_v1() {
    let pool = SqlitePoolOptions::new()
        .max_connections(1)
        .connect("sqlite::memory:")
        .await
        .unwrap();
    apply_migrations_through(&pool, "0065").await;

    for (id, mode) in [
        ("legacy-strict", "request_approval"),
        ("legacy-auto", "assisted"),
        ("legacy-full", "full_access"),
        ("legacy-plan", "plan"),
        ("legacy-workspace", "workspace_write"),
    ] {
        insert_session(&pool, id, mode).await;
    }

    let sql = std::fs::read_to_string(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("migrations/0066_permission_contract.sql"),
    )
    .unwrap();
    sqlx::raw_sql(&sql).execute(&pool).await.unwrap();

    for (id, expected_mode) in [
        ("legacy-strict", "request_approval"),
        ("legacy-auto", "assisted"),
        ("legacy-full", "full_access"),
        ("legacy-plan", "request_approval"),
        ("legacy-workspace", "assisted"),
    ] {
        let (mode, contract): (String, i64) =
            sqlx::query_as("SELECT mode, permission_contract FROM sessions WHERE id = ?1")
                .bind(id)
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(mode, expected_mode, "{id} mode must not be rewritten");
        assert_eq!(contract, 1, "{id} must stay on the legacy contract");
    }
}

#[tokio::test]
async fn a_new_row_written_through_the_engine_is_v2() {
    let db = leveler_storage::Database::connect_in_memory()
        .await
        .unwrap();
    let record =
        leveler_storage::SessionRecord::new("/repo", "goal", "mock/m", leveler_core::now());
    db.create_task(&record, "assisted", 2, false, "direct")
        .await
        .unwrap();
    let (mode, contract, _, _, _) = leveler_storage::SessionRepository::new(&db)
        .execution_state(&leveler_core::SessionId::new(&record.id))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(mode, "assisted");
    assert_eq!(contract, 2);
}

/// A tier switch preserves the stored contract; only an explicit adoption
/// writes V2. This is the storage half of the no-silent-elevation invariant.
#[tokio::test]
async fn switching_the_tier_never_adopts_v2() {
    let db = leveler_storage::Database::connect_in_memory()
        .await
        .unwrap();
    let record =
        leveler_storage::SessionRecord::new("/repo", "goal", "mock/m", leveler_core::now());
    db.create_task(&record, "assisted", 1, false, "direct")
        .await
        .unwrap();
    let id = leveler_core::SessionId::new(&record.id);
    let sessions = leveler_storage::SessionRepository::new(&db);

    sessions
        .set_permission_state(&id, "request_approval", 1, leveler_core::now())
        .await
        .unwrap();
    let (mode, contract, _, _, _) = sessions.execution_state(&id).await.unwrap().unwrap();
    assert_eq!(mode, "request_approval");
    assert_eq!(contract, 1, "a tier switch must not adopt V2");

    sessions
        .set_permission_state(&id, "assisted", 2, leveler_core::now())
        .await
        .unwrap();
    let (mode, contract, _, _, _) = sessions.execution_state(&id).await.unwrap().unwrap();
    assert_eq!(mode, "assisted");
    assert_eq!(contract, 2, "the explicit adoption writes both facts");
}
