//! Persistence for normalized model-request diagnostics.

use leveler_core::{SessionId, Timestamp};

use crate::{Database, StorageError};

/// One completed model call, recorded for diagnostics rather than replay: this
/// row holds no prompt or response text, only the shape of the exchange.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelRequestRecord {
    /// Caller-supplied primary key. The repository does not generate it and
    /// does not check it for collisions — a duplicate fails at the database.
    pub id: String,
    /// Owning session. Not stored on read-back from [`ModelRequestRepository::load_for_session`];
    /// it is filled in from the queried session instead.
    pub session_id: SessionId,
    /// Provider key as configured (e.g. `deepseek`), not the vendor's own name.
    pub provider: String,
    /// Provider-side model id actually sent on the wire, which may differ from
    /// the local alias the user typed.
    pub model: String,
    /// Prompt tokens reported by the provider. Persisted as a signed 64-bit
    /// integer, so a value above `i64::MAX` is clamped on write.
    pub input_tokens: u64,
    /// Completion tokens, clamped on write like [`Self::input_tokens`].
    pub output_tokens: u64,
    /// Provider's stop reason (`stop`, `length`, `tool_calls`, …). `None` when
    /// the call never reached a normal end — see [`Self::error_kind`].
    pub finish_reason: Option<String>,
    /// Failure classification when the call did not complete. Mutually
    /// exclusive with [`Self::finish_reason`] in practice, though nothing
    /// enforces that here.
    pub error_kind: Option<String>,
    /// Wall-clock duration of the call. `None` when it was never measured
    /// (for example a request that failed before being sent).
    pub latency_ms: Option<u64>,
    /// Retries *before* this outcome; `0` means it succeeded first try.
    pub retry_count: u32,
    /// When the call finished. Stored as RFC 3339 text and re-parsed on read,
    /// so an unparseable value surfaces as [`StorageError::InvalidData`].
    pub created_at: Timestamp,
}

/// Read/write access to the `model_requests` table, borrowed from a [`Database`].
pub struct ModelRequestRepository<'a> {
    db: &'a Database,
}

impl<'a> ModelRequestRepository<'a> {
    /// Borrow `db` for the lifetime of this repository handle.
    pub fn new(db: &'a Database) -> Self {
        Self { db }
    }

    /// Append one record. Token counts and latency are clamped into the signed
    /// range SQLite stores; no other normalization happens.
    ///
    /// # Errors
    ///
    /// Fails if the row cannot be written — including a duplicate
    /// [`ModelRequestRecord::id`], which the database rejects.
    pub async fn insert(&self, record: &ModelRequestRecord) -> Result<(), StorageError> {
        sqlx::query(
            "INSERT INTO model_requests \
             (id, session_id, provider, model, input_tokens, output_tokens, finish_reason, \
              error_kind, latency_ms, retry_count, created_at) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
        )
        .bind(&record.id)
        .bind(record.session_id.as_str())
        .bind(&record.provider)
        .bind(&record.model)
        .bind(record.input_tokens.min(i64::MAX as u64) as i64)
        .bind(record.output_tokens.min(i64::MAX as u64) as i64)
        .bind(&record.finish_reason)
        .bind(&record.error_kind)
        .bind(
            record
                .latency_ms
                .map(|value| value.min(i64::MAX as u64) as i64),
        )
        .bind(i64::from(record.retry_count))
        .bind(record.created_at.to_rfc3339())
        .execute(self.db.pool())
        .await?;
        Ok(())
    }

    /// Every record for `session_id`, oldest first (ties broken by insertion
    /// order, so calls made within the same second keep their sequence).
    ///
    /// # Errors
    ///
    /// Returns [`StorageError::InvalidData`] if a stored timestamp does not
    /// parse; the whole load fails rather than skipping the bad row.
    pub async fn load_for_session(
        &self,
        session_id: &SessionId,
    ) -> Result<Vec<ModelRequestRecord>, StorageError> {
        let rows = sqlx::query_as::<
            _,
            (
                String,
                String,
                String,
                i64,
                i64,
                Option<String>,
                Option<String>,
                Option<i64>,
                i64,
                String,
            ),
        >(
            "SELECT id, provider, model, input_tokens, output_tokens, finish_reason, error_kind, \
                    latency_ms, retry_count, created_at \
             FROM model_requests WHERE session_id = ?1 ORDER BY created_at, rowid",
        )
        .bind(session_id.as_str())
        .fetch_all(self.db.pool())
        .await?;

        rows.into_iter()
            .map(
                |(
                    id,
                    provider,
                    model,
                    input_tokens,
                    output_tokens,
                    finish_reason,
                    error_kind,
                    latency_ms,
                    retry_count,
                    created_at,
                )| {
                    Ok(ModelRequestRecord {
                        id,
                        session_id: session_id.clone(),
                        provider,
                        model,
                        input_tokens: input_tokens.max(0) as u64,
                        output_tokens: output_tokens.max(0) as u64,
                        finish_reason,
                        error_kind,
                        latency_ms: latency_ms.map(|value| value.max(0) as u64),
                        retry_count: retry_count.clamp(0, i64::from(u32::MAX)) as u32,
                        created_at: created_at.parse::<Timestamp>().map_err(|error| {
                            StorageError::InvalidData(format!("model request timestamp: {error}"))
                        })?,
                    })
                },
            )
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{SessionRecord, SessionRepository};

    #[tokio::test]
    async fn finish_reason_and_usage_are_persisted() {
        let db = Database::connect_in_memory().await.unwrap();
        let session = SessionRecord::new("/r", "g", "provider/model", leveler_core::now());
        SessionRepository::new(&db).create(&session).await.unwrap();
        let session_id = SessionId::new(session.id);
        let record = ModelRequestRecord {
            id: "req-1".to_string(),
            session_id: session_id.clone(),
            provider: "provider".to_string(),
            model: "model".to_string(),
            input_tokens: 100,
            output_tokens: 20,
            finish_reason: Some("length".to_string()),
            error_kind: None,
            latency_ms: Some(42),
            retry_count: 1,
            created_at: leveler_core::now(),
        };

        let repo = ModelRequestRepository::new(&db);
        repo.insert(&record).await.unwrap();
        let loaded = repo.load_for_session(&session_id).await.unwrap();

        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].finish_reason.as_deref(), Some("length"));
        assert_eq!(loaded[0].output_tokens, 20);
        assert_eq!(loaded[0].retry_count, 1);
    }
}
