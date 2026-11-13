//! SQLite-backed lexical index for memory content.
//!
//! The index stores chunked Markdown in `chunks` and mirrors it into a
//! contentless FTS5 table for BM25 keyword search. Memory content is local to
//! the machine and is never sent to an embedding provider.

use std::collections::HashMap;
use std::path::Path;

use rusqlite::params;

use crate::chunker::{chunk_hash, chunk_markdown};
use crate::config::MemoryIndexConfig;
use crate::schema;
use crate::sqlite_journal::JournalMode;
use crate::storage::MemoryStorage;

/// A record from the `chunks` table.
#[derive(Debug, Clone)]
pub struct ChunkRecord {
    pub rowid: i64,
    pub id: String,
    pub path: String,
    pub start_line: usize,
    pub end_line: usize,
    pub text: String,
    pub hash: String,
    pub source: String,
    pub access_count: i64,
    pub created_at: i64,
}

/// Result of an FTS5 keyword search.
#[derive(Debug, Clone)]
pub struct FtsResult {
    pub chunk_id: String,
    pub rowid: i64,
    pub rank: f64,
}

/// Result of reindexing a file.
#[derive(Debug, Clone, Copy, Default)]
pub struct ReindexResult {
    pub added: usize,
    pub updated: usize,
    pub removed: usize,
}

/// SQLite-backed memory index.
pub struct MemoryIndex {
    db: rusqlite::Connection,
    #[expect(
        dead_code,
        reason = "kept with the index handle for future scope-aware reads"
    )]
    storage: MemoryStorage,
    chunk_config: MemoryIndexConfig,
}

impl MemoryIndex {
    /// Open or create the index database at `db_path`.
    ///
    /// `_dimensions` is accepted for source compatibility with the
    /// embedding-capable index; this index is lexical-only.
    pub fn open_or_create(
        db_path: &Path,
        storage: MemoryStorage,
        config: MemoryIndexConfig,
        _dimensions: usize,
    ) -> Result<Self, rusqlite::Error> {
        if let Some(parent) = db_path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let journal_mode = JournalMode::for_db_path(db_path);
        Self::open_or_create_with_journal_mode(db_path, storage, config, journal_mode)
    }

    /// Open an index for lexical-only maintenance.
    ///
    /// The embedding dimension recorded by an older deployment is preserved
    /// untouched; the lexical tables do not depend on it.
    pub(crate) fn open_or_create_preserving_dimensions(
        db_path: &Path,
        storage: MemoryStorage,
        config: MemoryIndexConfig,
        _fallback_dimensions: usize,
    ) -> Result<Self, rusqlite::Error> {
        if let Some(parent) = db_path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let journal_mode = JournalMode::for_db_path(db_path);
        Self::open_or_create_with_journal_mode(db_path, storage, config, journal_mode)
    }

    fn open_or_create_with_journal_mode(
        db_path: &Path,
        storage: MemoryStorage,
        config: MemoryIndexConfig,
        journal_mode: JournalMode,
    ) -> Result<Self, rusqlite::Error> {
        let db = journal_mode.open(db_path)?;
        db.execute_batch(&schema::schema_sql())?;
        Ok(Self {
            db,
            storage,
            chunk_config: config,
        })
    }

    /// Reindex a single file from disk.
    pub fn reindex_file(
        &mut self,
        path: &Path,
        source: &str,
    ) -> Result<ReindexResult, rusqlite::Error> {
        let content = match std::fs::read_to_string(path) {
            Ok(content) => content,
            Err(error) => {
                tracing::warn!(path = %path.display(), %error, "failed to read file for reindexing");
                return Ok(ReindexResult::default());
            }
        };
        self.reindex_content(path, source, &content)
    }

    /// Reindex a path from bytes already read and validated by the caller.
    pub fn reindex_content(
        &mut self,
        path: &Path,
        source: &str,
        content: &str,
    ) -> Result<ReindexResult, rusqlite::Error> {
        let span = tracing::info_span!(
            "memory.reindex_file",
            bytes = tracing::field::Empty,
            chunk_count = tracing::field::Empty,
        );
        let _guard = span.enter();
        span.record("bytes", content.len() as i64);

        let new_chunks = chunk_markdown(content, &self.chunk_config);
        span.record("chunk_count", new_chunks.len() as i64);
        let path_str = path.to_string_lossy().to_string();
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs() as i64;

        let mut result = ReindexResult::default();
        let mut seen_ids = std::collections::HashSet::new();

        let tx = self
            .db
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let existing = Self::get_chunks_for_path(&tx, &path_str)?;

        for (index, chunk) in new_chunks.iter().enumerate() {
            let chunk_id = format!("{}:{}", path_str, index);
            let hash = chunk_hash(&chunk.text);
            seen_ids.insert(chunk_id.clone());

            match existing.get(&chunk_id) {
                Some(old) if old.hash == hash => {}
                Some(old) => {
                    tx.execute(
                        "UPDATE chunks SET text = ?1, hash = ?2, start_line = ?3, \
                         end_line = ?4, updated_at = ?5 WHERE id = ?6",
                        params![
                            chunk.text,
                            hash,
                            chunk.start_line,
                            chunk.end_line,
                            now,
                            chunk_id
                        ],
                    )?;
                    tx.execute(
                        "INSERT INTO chunks_fts(chunks_fts, rowid, text) VALUES('delete', ?1, ?2)",
                        params![old.rowid, old.text],
                    )?;
                    if let Ok(new_rowid) = tx.query_row(
                        "SELECT rowid FROM chunks WHERE id = ?1",
                        params![chunk_id],
                        |row| row.get::<_, i64>(0),
                    ) {
                        tx.execute(
                            "INSERT INTO chunks_fts(rowid, text) VALUES (?1, ?2)",
                            params![new_rowid, chunk.text],
                        )?;
                    }
                    result.updated += 1;
                }
                None => {
                    tx.execute(
                        "INSERT INTO chunks (id, path, start_line, end_line, text, hash, source, \
                         created_at, updated_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
                        params![
                            chunk_id,
                            path_str,
                            chunk.start_line,
                            chunk.end_line,
                            chunk.text,
                            hash,
                            source,
                            now,
                            now,
                        ],
                    )?;
                    let rowid = tx.last_insert_rowid();
                    tx.execute(
                        "INSERT INTO chunks_fts(rowid, text) VALUES (?1, ?2)",
                        params![rowid, chunk.text],
                    )?;
                    result.added += 1;
                }
            }
        }

        for (old_id, old_record) in &existing {
            if !seen_ids.contains(old_id) {
                tx.execute(
                    "INSERT INTO chunks_fts(chunks_fts, rowid, text) VALUES('delete', ?1, ?2)",
                    params![old_record.rowid, old_record.text],
                )?;
                tx.execute("DELETE FROM chunks WHERE id = ?1", params![old_id])?;
                result.removed += 1;
            }
        }

        tx.commit()?;
        Ok(result)
    }

    /// FTS5 keyword search across all sources.
    pub fn search_fts(&self, query: &str, limit: usize) -> Result<Vec<FtsResult>, rusqlite::Error> {
        let keywords = crate::query_expansion::extract_keywords(query);
        let fts_query = keywords.join(" OR ");
        if fts_query.is_empty() {
            return Ok(vec![]);
        }

        let _guard = tracing::info_span!("memory.fts_search").entered();
        let mut stmt = self.db.prepare(
            "SELECT rowid, rank FROM chunks_fts WHERE chunks_fts MATCH ?1 \
             ORDER BY rank LIMIT ?2",
        )?;
        let rows = stmt
            .query_map(params![fts_query, limit], |row| {
                Ok((row.get::<_, i64>(0)?, row.get::<_, f64>(1)?))
            })?
            .collect::<Result<Vec<_>, _>>()?;

        self.resolve_fts_rowids(rows)
    }

    /// FTS5 keyword search restricted to a set of source classes.
    pub fn search_fts_by_sources(
        &self,
        query: &str,
        limit: usize,
        sources: &[&str],
    ) -> Result<Vec<FtsResult>, rusqlite::Error> {
        if sources.is_empty() {
            return Ok(vec![]);
        }
        let keywords = crate::query_expansion::extract_keywords(query);
        let fts_query = keywords.join(" OR ");
        if fts_query.is_empty() {
            return Ok(vec![]);
        }

        let _guard = tracing::info_span!("memory.fts_search").entered();
        let placeholders: Vec<String> = sources
            .iter()
            .enumerate()
            .map(|(index, _)| format!("?{}", index + 3))
            .collect();
        let sql = format!(
            "SELECT f.rowid, f.rank FROM chunks_fts f \
             JOIN chunks c ON f.rowid = c.rowid \
             WHERE chunks_fts MATCH ?1 AND c.source IN ({}) \
             ORDER BY f.rank LIMIT ?2",
            placeholders.join(", ")
        );
        let mut stmt = self.db.prepare(&sql)?;
        let mut params_vec: Vec<Box<dyn rusqlite::types::ToSql>> = Vec::new();
        params_vec.push(Box::new(fts_query));
        params_vec.push(Box::new(limit as i64));
        for source in sources {
            params_vec.push(Box::new(source.to_string()));
        }
        let param_refs: Vec<&dyn rusqlite::types::ToSql> =
            params_vec.iter().map(|param| param.as_ref()).collect();

        let rows = stmt
            .query_map(param_refs.as_slice(), |row| {
                Ok((row.get::<_, i64>(0)?, row.get::<_, f64>(1)?))
            })?
            .collect::<Result<Vec<_>, _>>()?;

        self.resolve_fts_rowids(rows)
    }

    fn resolve_fts_rowids(&self, rows: Vec<(i64, f64)>) -> Result<Vec<FtsResult>, rusqlite::Error> {
        let mut results = Vec::with_capacity(rows.len());
        for (rowid, rank) in rows {
            if let Ok(chunk_id) = self.db.query_row(
                "SELECT id FROM chunks WHERE rowid = ?1",
                params![rowid],
                |row| row.get::<_, String>(0),
            ) {
                results.push(FtsResult {
                    chunk_id,
                    rowid,
                    rank,
                });
            }
        }
        Ok(results)
    }

    /// Get a chunk by its stable identifier.
    pub fn get_chunk(&self, id: &str) -> Result<Option<ChunkRecord>, rusqlite::Error> {
        let mut stmt = self.db.prepare(
            "SELECT rowid, id, path, start_line, end_line, text, hash, source, access_count, \
             created_at FROM chunks WHERE id = ?1",
        )?;
        let result = stmt
            .query_row(params![id], |row| {
                Ok(ChunkRecord {
                    rowid: row.get(0)?,
                    id: row.get(1)?,
                    path: row.get(2)?,
                    start_line: row.get::<_, i64>(3)? as usize,
                    end_line: row.get::<_, i64>(4)? as usize,
                    text: row.get(5)?,
                    hash: row.get(6)?,
                    source: row.get(7)?,
                    access_count: row.get(8)?,
                    created_at: row.get(9)?,
                })
            })
            .ok();
        Ok(result)
    }

    /// Return every distinct file path with at least one indexed chunk.
    pub fn all_indexed_paths(&self) -> Result<Vec<String>, rusqlite::Error> {
        let mut stmt = self
            .db
            .prepare("SELECT DISTINCT path FROM chunks ORDER BY path")?;
        let paths = stmt
            .query_map([], |row| row.get::<_, String>(0))?
            .filter_map(|row| row.ok())
            .collect();
        Ok(paths)
    }

    /// Record an access to a chunk.
    pub fn record_access(&mut self, chunk_id: &str) -> Result<(), rusqlite::Error> {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs() as i64;
        self.db.execute(
            "UPDATE chunks SET access_count = access_count + 1, last_accessed = ?1 WHERE id = ?2",
            params![now, chunk_id],
        )?;
        Ok(())
    }

    /// Take the exclusive reindex claim when it is free or stale.
    pub fn try_claim_reindex(&self, stale_threshold_secs: i64) -> bool {
        let pid = std::process::id();
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs() as i64;
        let claim_value = format!("{pid}:{now}");
        let stale_cutoff = now - stale_threshold_secs;

        let rows = self
            .db
            .execute(
                "UPDATE meta SET value = ?1 WHERE key = 'reindex_claim' \
             AND (value = '' OR CAST(SUBSTR(value, INSTR(value, ':') + 1) AS INTEGER) < ?2)",
                params![claim_value, stale_cutoff],
            )
            .unwrap_or_else(|error| {
                tracing::warn!(%error, "reindex claim SQL failed, treating as claim-fail");
                0
            });

        rows == 1
    }

    pub fn release_claim(&self) {
        let _ = self
            .db
            .execute("UPDATE meta SET value = '' WHERE key = 'reindex_claim'", []);
    }

    /// Delete every indexed chunk for a file path in one transaction.
    pub fn delete_path(&mut self, path: &Path) -> Result<usize, rusqlite::Error> {
        let path_str = path.to_string_lossy().to_string();
        let tx = self
            .db
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let existing = Self::get_chunks_for_path(&tx, &path_str)?;

        if existing.is_empty() {
            return Ok(0);
        }

        for (chunk_id, record) in &existing {
            tx.execute(
                "INSERT INTO chunks_fts(chunks_fts, rowid, text) VALUES('delete', ?1, ?2)",
                params![record.rowid, record.text],
            )?;
            tx.execute("DELETE FROM chunks WHERE id = ?1", params![chunk_id])?;
        }
        tx.commit()?;

        tracing::debug!(
            path = %path.display(),
            count = existing.len(),
            "memory index: deleted chunks for removed file"
        );
        Ok(existing.len())
    }

    fn get_chunks_for_path(
        connection: &rusqlite::Connection,
        path: &str,
    ) -> Result<HashMap<String, ChunkRecord>, rusqlite::Error> {
        let mut stmt = connection.prepare(
            "SELECT rowid, id, path, start_line, end_line, text, hash, source, access_count, \
             created_at FROM chunks WHERE path = ?1",
        )?;
        let rows = stmt.query_map(params![path], |row| {
            Ok(ChunkRecord {
                rowid: row.get(0)?,
                id: row.get(1)?,
                path: row.get(2)?,
                start_line: row.get::<_, i64>(3)? as usize,
                end_line: row.get::<_, i64>(4)? as usize,
                text: row.get(5)?,
                hash: row.get(6)?,
                source: row.get(7)?,
                access_count: row.get(8)?,
                created_at: row.get(9)?,
            })
        })?;
        let mut map = HashMap::new();
        for row in rows {
            let record = row?;
            map.insert(record.id.clone(), record);
        }
        Ok(map)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn index(root: &Path) -> MemoryIndex {
        MemoryIndex::open_or_create(
            &root.join("index.sqlite"),
            MemoryStorage::new_flat(root, root),
            MemoryIndexConfig::default(),
            1,
        )
        .unwrap()
    }

    #[test]
    fn reindex_and_search_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let mut index = index(dir.path());
        let file = dir.path().join("topics").join("build.md");
        std::fs::create_dir_all(file.parent().unwrap()).unwrap();
        std::fs::write(&file, "# Build\n\ncargo test all targets\n").unwrap();

        let result = index.reindex_file(&file, "workspace").unwrap();
        assert_eq!(result.added, 1);

        let results = index.search_fts("cargo test", 10).unwrap();
        assert_eq!(results.len(), 1);
        let first = results.first().expect("one match");
        let chunk = index.get_chunk(&first.chunk_id).unwrap().unwrap();
        assert!(!chunk.text.is_empty());
    }

    #[test]
    fn delete_path_removes_fts_entries() {
        let dir = tempfile::tempdir().unwrap();
        let mut index = index(dir.path());
        let file = dir.path().join("topics").join("build.md");
        std::fs::create_dir_all(file.parent().unwrap()).unwrap();
        std::fs::write(&file, "# Build\n\ncargo test all targets\n").unwrap();
        index.reindex_file(&file, "workspace").unwrap();
        assert_eq!(index.delete_path(&file).unwrap(), 1);
        assert!(index.search_fts("cargo", 10).unwrap().is_empty());
    }

    #[test]
    fn claim_is_exclusive_until_released() {
        let dir = tempfile::tempdir().unwrap();
        let index = index(dir.path());
        assert!(index.try_claim_reindex(60));
        assert!(!index.try_claim_reindex(60));
        index.release_claim();
        assert!(index.try_claim_reindex(60));
    }

    #[test]
    fn reindex_content_replaces_changed_chunks() {
        let dir = tempfile::tempdir().unwrap();
        let mut index = index(dir.path());
        let file = dir.path().join("topics").join("build.md");
        std::fs::create_dir_all(file.parent().unwrap()).unwrap();
        index
            .reindex_content(&file, "workspace", "# Build\n\nold command\n")
            .unwrap();
        let result = index
            .reindex_content(&file, "workspace", "# Build\n\nnew command\n")
            .unwrap();
        assert_eq!(result.updated, 1);
        assert!(index.search_fts("old", 10).unwrap().is_empty());
        assert_eq!(index.search_fts("new", 10).unwrap().len(), 1);
    }
}
