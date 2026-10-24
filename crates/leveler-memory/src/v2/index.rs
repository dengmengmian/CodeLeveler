//! SQLite FTS5 derived index over Markdown current records.
//!
//! Markdown is the only content authority. This database exists to make
//! lexical candidate selection cheap; every hit is re-read and re-validated
//! against Markdown before it can reach a caller. Deleting the database loses
//! nothing: [`MemoryIndex::reconcile`] rebuilds it from the effective records.
//!
//! The index never stores a record that is sensitive or credential-shaped, and
//! it never decides identity, status or supersession. Those come from the
//! Markdown record and its receipt.

use std::collections::{BTreeSet, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::Duration;

use rusqlite::{Connection, OptionalExtension, params};

use super::record::{MemoryKind, MemoryRecord, MemoryScope, MemoryStatus};
use super::store::EffectiveRecord;

/// Current index layout. A different value rebuilds the derived tables.
pub const INDEX_SCHEMA_VERSION: i64 = 2;
/// Tokenizer behavior. A different value also forces a rebuild.
pub const INDEX_TOKENIZER_VERSION: &str = "cjk-bigram-unigram-v1";
pub const INDEX_FILE: &str = "fts.sqlite";

#[derive(Debug, thiserror::Error)]
pub enum IndexError {
    #[error("memory index is degraded: {0}")]
    Degraded(String),
    #[error("memory index I/O failed: {0}")]
    Io(#[from] std::io::Error),
    #[error("memory index query failed: {0}")]
    Sqlite(rusqlite::Error),
}

impl From<rusqlite::Error> for IndexError {
    fn from(error: rusqlite::Error) -> Self {
        match error {
            rusqlite::Error::SqliteFailure(code, _)
                if code.code == rusqlite::ErrorCode::DatabaseBusy
                    || code.code == rusqlite::ErrorCode::DatabaseLocked =>
            {
                Self::Degraded("index is busy".into())
            }
            other => Self::Degraded(other.to_string()),
        }
    }
}

/// One ranked candidate. `score` is higher-is-better (the negative of bm25).
#[derive(Debug, Clone, PartialEq)]
pub struct IndexHit {
    pub id: String,
    pub score: f64,
}

/// One catalog/standing row derived from the index. It carries only the
/// fields selection needs; the body still comes from Markdown when a caller
/// reads the record.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DerivedRow {
    pub id: String,
    pub revision: u64,
    pub title: String,
    pub authority: String,
    pub pinned_by_user: bool,
    pub updated_at: String,
    pub subject_key: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ReconcileReport {
    pub indexed: usize,
    pub removed: usize,
    pub unchanged: usize,
    pub skipped_sensitive: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IndexDiagnostics {
    pub documents: usize,
    pub dirty: bool,
    pub schema_version: i64,
    pub tokenizer_version: String,
}

/// One derived row. Only non-sensitive current records become documents.
#[derive(Debug, Clone)]
struct Document {
    id: String,
    scope: String,
    kind: String,
    status: String,
    revision: u64,
    digest: String,
    title: String,
    subject: Option<String>,
    subject_key: Option<String>,
    value: Option<String>,
    tags: Vec<String>,
    authority: String,
    pinned: bool,
    pinned_by: Option<String>,
    updated_at: String,
    expires_at: Option<String>,
    title_terms: String,
    subject_terms: String,
    body_terms: String,
    tag_terms: String,
}

pub struct MemoryIndex {
    path: PathBuf,
    conn: Mutex<Connection>,
}

impl MemoryIndex {
    /// Open (or create) the index under `index_dir`.
    ///
    /// A corrupt or incompatible database is replaced by an empty one and the
    /// caller is told it is dirty, so the next reconcile rebuilds from
    /// Markdown. No Markdown file is touched here.
    pub fn open(index_dir: &Path) -> Result<Self, IndexError> {
        std::fs::create_dir_all(index_dir)?;
        let path = index_dir.join(INDEX_FILE);
        if let Ok(conn) = Self::open_connection(&path) {
            let index = Self {
                path: path.clone(),
                conn: Mutex::new(conn),
            };
            match index.initialize(false) {
                Ok(()) => return Ok(index),
                Err(IndexError::Sqlite(error)) if is_corruption(&error) => {}
                Err(error) => return Err(error),
            }
        }
        remove_database_files(&path)?;
        let conn = Self::open_connection(&path)?;
        let index = Self {
            path: path.clone(),
            conn: Mutex::new(conn),
        };
        index.initialize(true)?;
        Ok(index)
    }

    fn open_connection(path: &Path) -> Result<Connection, rusqlite::Error> {
        let conn = Connection::open(path)?;
        conn.busy_timeout(Duration::from_secs(5))?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        Ok(conn)
    }

    fn initialize(&self, rebuilt: bool) -> Result<(), IndexError> {
        let mut conn = self.lock();
        let healthy: Option<String> = conn
            .query_row("PRAGMA quick_check", [], |row| row.get(0))
            .optional()?;
        if healthy.as_deref() != Some("ok") {
            drop(conn);
            remove_database_files(&self.path)?;
            let fresh = Self::open_connection(&self.path)?;
            *self.conn.lock().unwrap_or_else(|error| error.into_inner()) = fresh;
            conn = self.lock();
        }
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS meta (
                 key TEXT PRIMARY KEY,
                 value TEXT NOT NULL
             );
             CREATE TABLE IF NOT EXISTS documents (
                 rowid INTEGER PRIMARY KEY,
                 id TEXT NOT NULL UNIQUE,
                 scope TEXT NOT NULL,
                 kind TEXT NOT NULL,
                 status TEXT NOT NULL,
                 revision INTEGER NOT NULL,
                 digest TEXT NOT NULL,
                 title TEXT NOT NULL,
                 subject TEXT,
                 subject_key TEXT,
                 value TEXT,
                 tags TEXT NOT NULL,
                 authority TEXT NOT NULL,
                 pinned INTEGER NOT NULL,
                 pinned_by TEXT,
                 updated_at TEXT NOT NULL,
                 expires_at TEXT,
                 title_terms TEXT NOT NULL,
                 subject_terms TEXT NOT NULL,
                 body_terms TEXT NOT NULL,
                 tag_terms TEXT NOT NULL
             );
             CREATE VIRTUAL TABLE IF NOT EXISTS documents_fts USING fts5(
                 title_terms,
                 subject_terms,
                 body_terms,
                 tag_terms,
                 content = 'documents',
                 content_rowid = 'rowid',
                 tokenize = 'unicode61'
             );
             CREATE INDEX IF NOT EXISTS documents_scope_kind_status
                 ON documents (scope, kind, status, pinned, authority, updated_at);",
        )?;

        let schema: Option<String> = conn
            .query_row(
                "SELECT value FROM meta WHERE key = 'schema_version'",
                [],
                |row| row.get(0),
            )
            .optional()?;
        let tokenizer: Option<String> = conn
            .query_row(
                "SELECT value FROM meta WHERE key = 'tokenizer_version'",
                [],
                |row| row.get(0),
            )
            .optional()?;
        let incompatible = schema.as_deref() != Some(&INDEX_SCHEMA_VERSION.to_string())
            || tokenizer.as_deref() != Some(INDEX_TOKENIZER_VERSION);
        if incompatible || rebuilt {
            conn.execute_batch("DELETE FROM documents_fts; DELETE FROM documents;")?;
            set_meta(&conn, "schema_version", &INDEX_SCHEMA_VERSION.to_string())?;
            set_meta(&conn, "tokenizer_version", INDEX_TOKENIZER_VERSION)?;
            set_meta(&conn, "dirty", "1")?;
            return Ok(());
        }
        Ok(())
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Connection> {
        self.conn.lock().unwrap_or_else(|error| error.into_inner())
    }

    pub fn is_dirty(&self) -> Result<bool, IndexError> {
        let conn = self.lock();
        let dirty: Option<String> = conn
            .query_row("SELECT value FROM meta WHERE key = 'dirty'", [], |row| {
                row.get(0)
            })
            .optional()?;
        Ok(dirty.as_deref() != Some("0"))
    }

    fn set_dirty(&self, dirty: bool) -> Result<(), IndexError> {
        let conn = self.lock();
        set_meta(&conn, "dirty", if dirty { "1" } else { "0" })
    }

    /// Rebuild or refresh the whole index from effective records.
    ///
    /// The caller passes the same set a retrieval might legally return:
    /// trusted, non-sensitive, active and unexpired.
    pub fn reconcile(&self, records: &[EffectiveRecord]) -> Result<ReconcileReport, IndexError> {
        let result = self.reconcile_inner(records);
        if result.is_err() {
            let _ = self.set_dirty(true);
        }
        result
    }

    fn reconcile_inner(&self, records: &[EffectiveRecord]) -> Result<ReconcileReport, IndexError> {
        let mut conn = self.lock();
        let transaction = conn.transaction()?;
        let existing: HashMap<String, String> = {
            let mut statement = transaction.prepare("SELECT id, digest FROM documents")?;
            let rows = statement.query_map([], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })?;
            rows.collect::<Result<_, _>>()?
        };
        let mut seen = HashSet::with_capacity(records.len());
        let mut report = ReconcileReport::default();
        for record in records {
            let id = record.record.frontmatter.id.to_string();
            seen.insert(id.clone());
            match document_of(record) {
                Some(document) if existing.get(&id) != Some(&document.digest) => {
                    write_document(&transaction, &document)?;
                    report.indexed += 1;
                }
                Some(_) => report.unchanged += 1,
                None => {
                    report.skipped_sensitive += 1;
                    if existing.contains_key(&id) {
                        delete_document(&transaction, &id)?;
                        report.removed += 1;
                    }
                }
            }
        }
        for id in existing.keys() {
            if !seen.contains(id) {
                delete_document(&transaction, id)?;
                report.removed += 1;
            }
        }
        transaction.commit()?;
        drop(conn);
        if report.indexed > 0 || report.removed > 0 {
            // A rebuilt index is a pile of small b-tree segments; merging them
            // once makes every later query cheaper. Derived state only.
            let conn = self.lock();
            conn.execute(
                "INSERT INTO documents_fts(documents_fts) VALUES('optimize')",
                [],
            )?;
        }
        self.set_dirty(false)?;
        Ok(report)
    }

    /// Bring one record in line with Markdown after a committed write.
    pub fn sync_record(&self, record: &EffectiveRecord) -> Result<(), IndexError> {
        let result = (|| {
            let document = document_of(record);
            let mut conn = self.lock();
            let transaction = conn.transaction()?;
            match document {
                Some(document) => write_document(&transaction, &document)?,
                None => delete_document(&transaction, &record.record.frontmatter.id.to_string())?,
            }
            transaction.commit()?;
            Ok(())
        })();
        if result.is_err() {
            // Markdown already committed. Never roll it back for derived state;
            // record that retrieval must rebuild before trusting the index.
            let _ = self.set_dirty(true);
        }
        result
    }

    /// Remove one identity from the derived index (forget, supersede,
    /// deprecate, expiry).
    pub fn remove(&self, id: &str) -> Result<(), IndexError> {
        let result = (|| {
            let mut conn = self.lock();
            let transaction = conn.transaction()?;
            delete_document(&transaction, id)?;
            transaction.commit()?;
            Ok(())
        })();
        if result.is_err() {
            let _ = self.set_dirty(true);
        }
        result
    }

    /// Lexical candidates ordered by bm25, restricted to the lanes that asked.
    ///
    /// The lane filter belongs in the query: ranking every current record and
    /// discarding the other lane's rows afterwards pays bm25 for candidates the
    /// caller can never use. `MemoryLanes::ALL` ranks both lanes together, so a
    /// dual-lane turn still resolves one shared ranking; a single-lane turn only
    /// pays for its own rows.
    pub fn search(
        &self,
        query: &str,
        limit: usize,
        lanes: crate::MemoryLanes,
    ) -> Result<Vec<IndexHit>, IndexError> {
        let terms = query_terms(query);
        if terms.is_empty() || limit == 0 || !lanes.any() {
            return Ok(Vec::new());
        }
        let kind = match (lanes.user, lanes.experience) {
            (true, true) => None,
            (true, false) => Some("user_fact"),
            (false, true) => Some("experience"),
            (false, false) => None,
        };
        let expression = terms
            .iter()
            .map(|term| format!("\"{}\"", term.replace('"', "\"\"")))
            .collect::<Vec<_>>()
            .join(" OR ");
        let conn = self.lock();
        // `?2` is the lane's kind, or NULL for both lanes. The predicate is
        // evaluated before the sort, so a single-lane query never computes bm25
        // for the other lane's rows.
        let mut statement = conn.prepare(
            "SELECT documents.id, bm25(documents_fts, 3.0, 2.0, 1.0, 1.5) AS rank
             FROM documents_fts
             JOIN documents ON documents.rowid = documents_fts.rowid
             WHERE documents_fts MATCH ?1
               AND (?2 IS NULL OR documents.kind = ?2)
             ORDER BY rank
             LIMIT ?3",
        )?;
        let rows = statement.query_map(params![expression, kind, limit as i64], |row| {
            let id: String = row.get(0)?;
            let rank: f64 = row.get(1)?;
            Ok(IndexHit { id, score: -rank })
        })?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    /// Catalog rows for one scope: active, non-sensitive user facts stated by
    /// the user. Experience, derived and model-inferred records never enter
    /// the cache-stable catalog.
    pub fn catalog_rows(
        &self,
        scope: MemoryScope,
        limit: usize,
    ) -> Result<Vec<DerivedRow>, IndexError> {
        self.derived_rows(
            scope,
            "kind = 'user_fact' AND status = 'active'",
            "authority IN ('user_explicit', 'user_approved', 'user_stated')",
            limit,
        )
    }

    /// Standing rows: user-pinned records plus user-stated/approved facts.
    /// A model pin is only a ranking hint and never a standing constraint.
    pub fn standing_rows(
        &self,
        scope: MemoryScope,
        limit: usize,
    ) -> Result<Vec<DerivedRow>, IndexError> {
        self.derived_rows(
            scope,
            "kind = 'user_fact' AND status = 'active'",
            "(pinned_by = 'user' OR authority IN ('user_explicit', 'user_approved', 'user_stated'))",
            limit,
        )
    }

    fn derived_rows(
        &self,
        scope: MemoryScope,
        kind_status: &str,
        selection: &str,
        limit: usize,
    ) -> Result<Vec<DerivedRow>, IndexError> {
        if limit == 0 {
            return Ok(Vec::new());
        }
        let conn = self.lock();
        let sql = format!(
            "SELECT id, revision, title, authority, pinned_by, updated_at, subject_key
             FROM documents
             WHERE scope = ?1 AND {kind_status} AND {selection}
             ORDER BY pinned DESC,
                      CASE authority
                          WHEN 'user_explicit' THEN 3
                          WHEN 'user_approved' THEN 2
                          WHEN 'user_stated' THEN 1
                          ELSE 0
                      END DESC,
                      updated_at DESC, id
             LIMIT ?2"
        );
        let mut statement = conn.prepare(&sql)?;
        let rows = statement.query_map(params![scope_str(scope), limit as i64], |row| {
            let pinned_by: Option<String> = row.get(4)?;
            Ok(DerivedRow {
                id: row.get(0)?,
                revision: row.get::<_, i64>(1)? as u64,
                title: row.get(2)?,
                authority: row.get(3)?,
                pinned_by_user: pinned_by.as_deref() == Some("user"),
                updated_at: row.get(5)?,
                subject_key: row.get(6)?,
            })
        })?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    /// The stored revision and digest for one id, used to detect stale rows.
    pub fn stored(&self, id: &str) -> Result<Option<(u64, String)>, IndexError> {
        let conn = self.lock();
        conn.query_row(
            "SELECT revision, digest FROM documents WHERE id = ?1",
            params![id],
            |row| Ok((row.get::<_, i64>(0)? as u64, row.get(1)?)),
        )
        .optional()
        .map_err(Into::into)
    }

    /// The stored revision and digest for several ids, read under one lock.
    ///
    /// Positions match `ids`; a row that is not indexed is `None`. Retrieval
    /// uses this so validating a candidate set costs one lock, not one per hit.
    pub fn stored_many(&self, ids: &[&str]) -> Result<Vec<Option<(u64, String)>>, IndexError> {
        let conn = self.lock();
        let mut statement = conn.prepare("SELECT revision, digest FROM documents WHERE id = ?1")?;
        let mut stored = Vec::with_capacity(ids.len());
        for id in ids {
            stored.push(
                statement
                    .query_row(params![id], |row| {
                        Ok((row.get::<_, i64>(0)? as u64, row.get(1)?))
                    })
                    .optional()?,
            );
        }
        Ok(stored)
    }

    /// Whether one scope holds any active record of this kind is not needed by
    /// retrieval: one lexical pass returns both lanes.
    pub fn diagnostics(&self) -> Result<IndexDiagnostics, IndexError> {
        let conn = self.lock();
        let documents: i64 =
            conn.query_row("SELECT COUNT(*) FROM documents", [], |row| row.get(0))?;
        let dirty: Option<String> = conn
            .query_row("SELECT value FROM meta WHERE key = 'dirty'", [], |row| {
                row.get(0)
            })
            .optional()?;
        Ok(IndexDiagnostics {
            documents: documents.max(0) as usize,
            dirty: dirty.as_deref() != Some("0"),
            schema_version: INDEX_SCHEMA_VERSION,
            tokenizer_version: INDEX_TOKENIZER_VERSION.to_string(),
        })
    }
}

fn set_meta(conn: &Connection, key: &str, value: &str) -> Result<(), IndexError> {
    conn.execute(
        "INSERT INTO meta(key, value) VALUES(?1, ?2)
         ON CONFLICT(key) DO UPDATE SET value = excluded.value",
        params![key, value],
    )?;
    Ok(())
}

/// The row identity and term values the external-content FTS table needs for
/// an update or delete. Reading by the UNIQUE id is O(log n); deleting through
/// the FTS table by any other column would scan every document.
struct StoredRow {
    rowid: i64,
    title_terms: String,
    subject_terms: String,
    body_terms: String,
    tag_terms: String,
}

fn stored_row(conn: &Connection, id: &str) -> Result<Option<StoredRow>, IndexError> {
    conn.query_row(
        "SELECT rowid, title_terms, subject_terms, body_terms, tag_terms
         FROM documents WHERE id = ?1",
        params![id],
        |row| {
            Ok(StoredRow {
                rowid: row.get(0)?,
                title_terms: row.get(1)?,
                subject_terms: row.get(2)?,
                body_terms: row.get(3)?,
                tag_terms: row.get(4)?,
            })
        },
    )
    .optional()
    .map_err(Into::into)
}

fn delete_fts_row(conn: &Connection, row: &StoredRow) -> Result<(), IndexError> {
    conn.execute(
        "INSERT INTO documents_fts(
             documents_fts, rowid, title_terms, subject_terms, body_terms, tag_terms
         ) VALUES('delete', ?1, ?2, ?3, ?4, ?5)",
        params![
            row.rowid,
            row.title_terms,
            row.subject_terms,
            row.body_terms,
            row.tag_terms,
        ],
    )?;
    Ok(())
}

fn insert_fts_row(conn: &Connection, rowid: i64, document: &Document) -> Result<(), IndexError> {
    conn.execute(
        "INSERT INTO documents_fts(
             rowid, title_terms, subject_terms, body_terms, tag_terms
         ) VALUES(?1, ?2, ?3, ?4, ?5)",
        params![
            rowid,
            document.title_terms,
            document.subject_terms,
            document.body_terms,
            document.tag_terms,
        ],
    )?;
    Ok(())
}

fn write_document(conn: &Connection, document: &Document) -> Result<(), IndexError> {
    let existing = stored_row(conn, &document.id)?;
    if let Some(existing) = &existing {
        delete_fts_row(conn, existing)?;
        conn.execute(
            "UPDATE documents SET
                 scope = ?2, kind = ?3, status = ?4, revision = ?5, digest = ?6,
                 title = ?7, subject = ?8, subject_key = ?9, value = ?10, tags = ?11,
                 authority = ?12, pinned = ?13, pinned_by = ?14, updated_at = ?15,
                 expires_at = ?16, title_terms = ?17, subject_terms = ?18,
                 body_terms = ?19, tag_terms = ?20
             WHERE rowid = ?1",
            params![
                existing.rowid,
                document.scope,
                document.kind,
                document.status,
                document.revision as i64,
                document.digest,
                document.title,
                document.subject,
                document.subject_key,
                document.value,
                serde_json::to_string(&document.tags).unwrap_or_else(|_| "[]".into()),
                document.authority,
                if document.pinned { 1i64 } else { 0 },
                document.pinned_by,
                document.updated_at,
                document.expires_at,
                document.title_terms,
                document.subject_terms,
                document.body_terms,
                document.tag_terms,
            ],
        )?;
        insert_fts_row(conn, existing.rowid, document)?;
        return Ok(());
    }
    conn.execute(
        "INSERT INTO documents(
             id, scope, kind, status, revision, digest, title, subject, subject_key,
             value, tags, authority, pinned, pinned_by, updated_at, expires_at,
             title_terms, subject_terms, body_terms, tag_terms
         ) VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16,
                  ?17, ?18, ?19, ?20)",
        params![
            document.id,
            document.scope,
            document.kind,
            document.status,
            document.revision as i64,
            document.digest,
            document.title,
            document.subject,
            document.subject_key,
            document.value,
            serde_json::to_string(&document.tags).unwrap_or_else(|_| "[]".into()),
            document.authority,
            if document.pinned { 1i64 } else { 0 },
            document.pinned_by,
            document.updated_at,
            document.expires_at,
            document.title_terms,
            document.subject_terms,
            document.body_terms,
            document.tag_terms,
        ],
    )?;
    let rowid = conn.last_insert_rowid();
    insert_fts_row(conn, rowid, document)?;
    Ok(())
}

fn delete_document(conn: &Connection, id: &str) -> Result<(), IndexError> {
    let Some(existing) = stored_row(conn, id)? else {
        return Ok(());
    };
    delete_fts_row(conn, &existing)?;
    conn.execute(
        "DELETE FROM documents WHERE rowid = ?1",
        params![existing.rowid],
    )?;
    Ok(())
}

fn remove_database_files(path: &Path) -> Result<(), IndexError> {
    for suffix in ["", "-wal", "-shm"] {
        let candidate = PathBuf::from(format!("{}{suffix}", path.display()));
        if candidate.exists() {
            std::fs::remove_file(&candidate)?;
        }
    }
    Ok(())
}

fn is_corruption(error: &rusqlite::Error) -> bool {
    // SQLITE_NOTADB is a structured code; the message text is display only.
    matches!(
        error,
        rusqlite::Error::SqliteFailure(code, _) if code.code == rusqlite::ErrorCode::NotADatabase
    )
}

/// Index terms: the shared deterministic tokenizer plus CJK unigrams.
///
/// The tokenizer emits bigrams for CJK runs; unigrams are added only on the
/// indexing side so a one-character query can still reach a longer term. No
/// word list, synonym dictionary or language model participates.
pub fn index_terms(text: &str) -> Vec<String> {
    let mut terms: BTreeSet<String> = crate::util::tokenize(text).into_iter().collect();
    let mut run = Vec::new();
    let flush = |run: &mut Vec<char>, terms: &mut BTreeSet<String>| {
        for character in run.drain(..) {
            terms.insert(character.to_string());
        }
    };
    for character in text.chars() {
        if crate::util::is_cjk(character) {
            run.push(character);
        } else {
            flush(&mut run, &mut terms);
        }
    }
    flush(&mut run, &mut terms);
    terms.into_iter().collect()
}

/// Query terms use exactly the shared tokenizer; indexing adds the unigrams.
pub fn query_terms(text: &str) -> Vec<String> {
    let terms: BTreeSet<String> = crate::util::tokenize(text).into_iter().collect();
    terms.into_iter().collect()
}

fn document_of(record: &EffectiveRecord) -> Option<Document> {
    let memory: &MemoryRecord = &record.record;
    let frontmatter = &memory.frontmatter;
    if is_sensitive_record(memory) {
        return None;
    }
    let title_terms = index_terms(&frontmatter.title).join(" ");
    let mut subject_text = String::new();
    if let Some(subject) = &frontmatter.subject {
        subject_text.push_str(subject);
        subject_text.push(' ');
    }
    if let Some(subject_key) = &frontmatter.subject_key {
        subject_text.push_str(subject_key);
        subject_text.push(' ');
    }
    if let Some(value) = &frontmatter.value {
        subject_text.push_str(value);
    }
    let subject_terms = index_terms(&subject_text).join(" ");
    let mut body_text = memory.body.clone();
    if let Some(experience) = &frontmatter.experience {
        body_text.push(' ');
        body_text.push_str(&experience.category);
        body_text.push(' ');
        body_text.push_str(&experience.applies_when);
        if let Some(invalid_if) = &experience.invalid_if {
            body_text.push(' ');
            body_text.push_str(invalid_if);
        }
    }
    let body_terms = index_terms(&body_text).join(" ");
    let tag_terms = index_terms(&frontmatter.tags.join(" ")).join(" ");
    Some(Document {
        id: frontmatter.id.to_string(),
        scope: scope_str(frontmatter.scope).into(),
        kind: kind_str(frontmatter.kind).into(),
        status: status_str(frontmatter.status).into(),
        revision: frontmatter.revision,
        digest: record.digest.clone(),
        title: frontmatter.title.clone(),
        subject: frontmatter.subject.clone(),
        subject_key: frontmatter.subject_key.clone(),
        value: frontmatter.value.clone(),
        tags: frontmatter.tags.clone(),
        authority: record.effective_authority.as_str().into(),
        pinned: frontmatter.pinned,
        pinned_by: frontmatter.pinned_by.map(|owner| match owner {
            super::record::PinOwner::User => "user".to_string(),
            super::record::PinOwner::Model => "model".to_string(),
        }),
        updated_at: frontmatter.updated_at.to_rfc3339(),
        expires_at: frontmatter.expires_at.map(|value| value.to_rfc3339()),
        title_terms,
        subject_terms,
        body_terms,
        tag_terms,
    })
}

/// Whether a current Markdown record may appear in retrieval at all.
///
/// Frontmatter sensitivity is authoritative; credential-shaped content is
/// excluded even when a writer forgot to set the flag, so it never enters the
/// derived database.
pub(crate) fn is_sensitive_record(record: &MemoryRecord) -> bool {
    let frontmatter = &record.frontmatter;
    frontmatter.sensitive || secret_shaped(frontmatter, &record.body)
}

fn secret_shaped(frontmatter: &super::record::Frontmatter, body: &str) -> bool {
    crate::looks_like_secret(&frontmatter.title)
        || crate::looks_like_secret(body)
        || frontmatter
            .subject
            .as_deref()
            .is_some_and(crate::looks_like_secret)
        || frontmatter
            .subject_key
            .as_deref()
            .is_some_and(crate::looks_like_secret)
        || frontmatter
            .value
            .as_deref()
            .is_some_and(crate::looks_like_secret)
        || frontmatter
            .tags
            .iter()
            .any(|tag| crate::looks_like_secret(tag))
}

fn scope_str(scope: super::record::MemoryScope) -> &'static str {
    match scope {
        super::record::MemoryScope::Global => "global",
        super::record::MemoryScope::Project => "project",
        super::record::MemoryScope::Local => "local",
    }
}

fn kind_str(kind: MemoryKind) -> &'static str {
    match kind {
        MemoryKind::UserFact => "user_fact",
        MemoryKind::Experience => "experience",
    }
}

fn status_str(status: MemoryStatus) -> &'static str {
    match status {
        MemoryStatus::Active => "active",
        MemoryStatus::Pending => "pending",
        MemoryStatus::Superseded => "superseded",
        MemoryStatus::Deprecated => "deprecated",
        MemoryStatus::Expired => "expired",
    }
}
