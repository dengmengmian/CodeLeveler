//! Persistence for normalized model-request diagnostics.

use leveler_core::{SessionId, Timestamp};

use crate::{Database, StorageError};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelRequestRecord {
    pub id: String,
    pub session_id: SessionId,
    pub provider: String,
    pub model: String,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub finish_reason: Option<String>,
    pub error_kind: Option<String>,
    pub latency_ms: Option<u64>,
    pub retry_count: u32,
    pub created_at: Timestamp,
}

pub struct ModelRequestRepository<'a> {
    db: &'a Database,
}

impl<'a> ModelRequestRepository<'a> {
    pub fn new(db: &'a Database) -> Self {
        Self { db }
    }

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
