//! SQL schema for the lexical memory index.
//!
//! Three tables: `meta` (key/value state), `chunks` (indexed text with blake3
//! content hashes), and the contentless FTS5 table `chunks_fts`. The index is
//! lexical-only by design; memory content is never sent to an embedding
//! service.

/// Bump when a breaking schema change requires dropping and recreating tables.
pub const SCHEMA_VERSION: u32 = 1;

/// Generate the SQL schema for the memory index.
pub fn schema_sql() -> String {
    r#"
CREATE TABLE IF NOT EXISTS meta (
    key TEXT PRIMARY KEY,
    value TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS chunks (
    rowid INTEGER PRIMARY KEY AUTOINCREMENT,
    id TEXT UNIQUE NOT NULL,
    path TEXT NOT NULL,
    start_line INTEGER NOT NULL,
    end_line INTEGER NOT NULL,
    text TEXT NOT NULL,
    hash TEXT NOT NULL,
    source TEXT NOT NULL,
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL,
    access_count INTEGER DEFAULT 0,
    last_accessed INTEGER
);

CREATE INDEX IF NOT EXISTS idx_chunks_path ON chunks(path);
CREATE INDEX IF NOT EXISTS idx_chunks_hash ON chunks(hash);

CREATE VIRTUAL TABLE IF NOT EXISTS chunks_fts USING fts5(text, content='');

INSERT OR IGNORE INTO meta(key, value) VALUES ('reindex_claim', '');
"#
    .to_string()
}

/// SQL to insert or replace a metadata record.
pub const UPSERT_META_SQL: &str = "INSERT OR REPLACE INTO meta(key, value) VALUES (?1, ?2)";

pub const GET_META_SQL: &str = "SELECT value FROM meta WHERE key = ?1";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn schema_has_chunks_and_fts_without_vector_tables() {
        let sql = schema_sql();
        assert!(sql.contains("CREATE TABLE IF NOT EXISTS chunks"));
        assert!(sql.contains("CREATE VIRTUAL TABLE IF NOT EXISTS chunks_fts"));
        assert!(!sql.contains("chunks_vec"));
        assert!(!sql.contains("PRAGMA"));
    }
}
