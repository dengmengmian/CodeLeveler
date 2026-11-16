//! Durable journal for accepted-but-not-yet-delivered user input.
//!
//! The transcript is only written when the model actually receives a message.
//! A steer, however, is acknowledged before the next round can consume it, so
//! the acknowledgement needs its own durable record. This repository owns that
//! record and its state machine:
//!
//! `pending` --(transcript append, same transaction)--> `consumed`
//! `pending` --(explicit cancel)--> `cancelled`
//!
//! A row is never deleted. A terminal row is the audit trail of what happened
//! to text the user typed.

use leveler_core::{SessionId, Timestamp, TurnId};

use crate::database::{Database, StorageError};

/// Where one accepted input is in its lifecycle.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionInputState {
    /// Accepted and durable; no model has seen it yet.
    Pending,
    /// Its transcript row was written in the same transaction.
    Consumed,
    /// Settled without delivery. Never resurrected.
    Cancelled,
}

/// One compare-and-set operation on a still-pending instruction.
#[derive(Clone, Copy)]
pub enum SessionInputMutation<'a> {
    /// Reserve the instruction for one editor until the given deadline.
    BeginEdit {
        /// Opaque capability held by the editor.
        lease_id: &'a str,
        /// Unix milliseconds at which this reservation expires.
        expires_ms: i64,
    },
    /// Replace the content under the active lease and release it.
    SaveEdit {
        /// The active edit capability.
        lease_id: &'a str,
        /// The new user-authored instruction.
        content: &'a str,
    },
    /// Release the active lease without changing the content.
    CancelEdit {
        /// The active edit capability.
        lease_id: &'a str,
    },
    /// Settle the instruction without delivery.
    Withdraw,
    /// Request early delivery while preserving FIFO order.
    Prioritize,
}

impl SessionInputState {
    /// Stable database spelling.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Consumed => "consumed",
            Self::Cancelled => "cancelled",
        }
    }

    fn parse(value: &str) -> Result<Self, StorageError> {
        match value {
            "pending" => Ok(Self::Pending),
            "consumed" => Ok(Self::Consumed),
            "cancelled" => Ok(Self::Cancelled),
            other => Err(StorageError::InvalidData(format!(
                "unknown session input state {other:?}"
            ))),
        }
    }
}

/// One accepted user input.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionInputRecord {
    /// Stable identity chosen by the admitting client (or the runtime when a
    /// legacy client omits one).
    pub id: String,
    /// Session the input belongs to.
    pub session_id: SessionId,
    /// Turn the input was admitted against, when known.
    pub turn_id: Option<TurnId>,
    /// Immutable window at admission. Older consumed inputs may not have one.
    pub origin_turn_id: Option<TurnId>,
    /// The user's text, redacted before write.
    pub content: String,
    /// Current lifecycle state.
    pub state: SessionInputState,
    /// Cancellation reason; empty for other states.
    pub reason: String,
    /// Transcript ordinal once consumed.
    pub ordinal: Option<u64>,
    /// RFC 3339 admission time.
    pub created_at: String,
    /// RFC 3339 time the row reached a terminal state.
    pub settled_at: Option<String>,
    /// Version of the pending state, including state-only operations.
    pub revision: i64,
    /// Deadline of the current edit reservation in Unix milliseconds.
    pub edit_expires_ms: Option<i64>,
    /// Whether the user requested acceleration.
    pub priority: bool,
}

impl SessionInputRecord {
    /// An active lease reserves the pending instruction until its deadline.
    pub fn editing_at(&self, at: Timestamp) -> bool {
        self.edit_expires_ms
            .is_some_and(|deadline| deadline > at.timestamp_millis())
    }
}

/// Read/write access to the `session_inputs` table.
pub struct SessionInputRepository<'a> {
    db: &'a Database,
}

const COLUMNS: &str = "id, session_id, turn_id, origin_turn_id, content, state, reason, ordinal, \
                       created_at, settled_at, revision, edit_expires_ms, priority";

fn decode(row: &sqlx::sqlite::SqliteRow) -> Result<SessionInputRecord, StorageError> {
    use sqlx::Row;
    let id: String = row.try_get("id")?;
    let session_id: String = row.try_get("session_id")?;
    let turn_id: Option<String> = row.try_get("turn_id")?;
    let origin_turn_id: Option<String> = row.try_get("origin_turn_id")?;
    let content: String = row.try_get("content")?;
    let state: String = row.try_get("state")?;
    let reason: String = row.try_get("reason")?;
    let ordinal: Option<i64> = row.try_get("ordinal")?;
    let created_at: String = row.try_get("created_at")?;
    let settled_at: Option<String> = row.try_get("settled_at")?;
    Ok(SessionInputRecord {
        id,
        session_id: SessionId::new(session_id),
        turn_id: turn_id.map(TurnId::new),
        origin_turn_id: origin_turn_id.map(TurnId::new),
        content,
        state: SessionInputState::parse(&state)?,
        reason,
        ordinal: ordinal.map(|value| value as u64),
        created_at,
        settled_at,
        revision: row.try_get("revision")?,
        edit_expires_ms: row.try_get("edit_expires_ms")?,
        priority: row.try_get("priority")?,
    })
}

impl<'a> SessionInputRepository<'a> {
    /// Borrow `db` for the lifetime of this repository handle.
    pub fn new(db: &'a Database) -> Self {
        Self { db }
    }

    /// Record an accepted input. Idempotent by `id`: a client retrying an
    /// unacknowledged submission cannot create a second row.
    pub async fn admit(
        &self,
        id: &str,
        session_id: &SessionId,
        turn_id: Option<&TurnId>,
        content: &str,
        now: Timestamp,
    ) -> Result<SessionInputRecord, StorageError> {
        // One value-position sanitizer owns user input. The legacy whole-line
        // scrubber rewrote source identifiers (let password = config.password;)
        // before the model ever saw the request.
        let (content, found) = leveler_core::sanitize_model_visible(content);
        if !found.is_empty() {
            leveler_core::register_session_secrets(session_id.as_str(), &found);
        }
        sqlx::query(
            "INSERT INTO session_inputs \
             (id, session_id, turn_id, origin_turn_id, content, state, reason, ordinal, created_at) \
             VALUES (?1, ?2, ?3, ?3, ?4, 'pending', '', NULL, ?5) \
             ON CONFLICT(id) DO NOTHING",
        )
        .bind(id)
        .bind(session_id.as_str())
        .bind(turn_id.map(TurnId::as_str))
        .bind(&content)
        .bind(now.to_rfc3339())
        .execute(self.db.pool())
        .await?;
        let row = sqlx::query(&format!(
            "SELECT {COLUMNS} FROM session_inputs WHERE id = ?1"
        ))
        .bind(id)
        .fetch_optional(self.db.pool())
        .await?
        .ok_or_else(|| StorageError::InvalidData("admitted input disappeared".into()))?;
        let record = decode(&row)?;
        if &record.session_id != session_id {
            return Err(StorageError::InvalidData(format!(
                "input id {id:?} belongs to another session"
            )));
        }
        Ok(record)
    }

    /// One input by identity, whatever its state.
    pub async fn load(
        &self,
        session_id: &SessionId,
        id: &str,
    ) -> Result<Option<SessionInputRecord>, StorageError> {
        let row = sqlx::query(&format!(
            "SELECT {COLUMNS} FROM session_inputs WHERE session_id = ?1 AND id = ?2"
        ))
        .bind(session_id.as_str())
        .bind(id)
        .fetch_optional(self.db.pool())
        .await?;
        row.as_ref().map(decode).transpose()
    }

    /// Mutate the matching pending revision. A lost lease, stale revision or
    /// already settled row returns `None` without changing durable state.
    pub async fn mutate(
        &self,
        session_id: &SessionId,
        id: &str,
        revision: i64,
        action: SessionInputMutation<'_>,
        now: Timestamp,
    ) -> Result<Option<SessionInputRecord>, StorageError> {
        let millis = now.timestamp_millis();
        let mut query = sqlx::QueryBuilder::new("UPDATE session_inputs SET ");
        let mut secrets = Vec::new();
        if !matches!(action, SessionInputMutation::Prioritize) {
            query.push("revision = revision + 1, ");
        }
        match action {
            SessionInputMutation::BeginEdit {
                lease_id,
                expires_ms,
            } => {
                query
                    .push("edit_id = ")
                    .push_bind(lease_id)
                    .push(", edit_expires_ms = ")
                    .push_bind(expires_ms);
            }
            SessionInputMutation::SaveEdit { content, .. } => {
                let (text, found) = leveler_core::sanitize_model_visible(content);
                secrets = found;
                query
                    .push("content = ")
                    .push_bind(text)
                    .push(", edit_id = NULL, edit_expires_ms = NULL");
            }
            SessionInputMutation::CancelEdit { .. } => {
                query.push("edit_id = NULL, edit_expires_ms = NULL");
            }
            SessionInputMutation::Withdraw => {
                query.push("state = 'cancelled', reason = 'withdrawn', edit_id = NULL, edit_expires_ms = NULL, settled_at = ")
                    .push_bind(now.to_rfc3339());
            }
            SessionInputMutation::Prioritize => {
                query.push("priority = 1");
            }
        }
        query
            .push(" WHERE session_id = ")
            .push_bind(session_id.as_str())
            .push(" AND id = ")
            .push_bind(id)
            .push(" AND revision = ")
            .push_bind(revision)
            .push(" AND state = 'pending'");
        match action {
            SessionInputMutation::BeginEdit { .. } | SessionInputMutation::Withdraw => {
                query
                    .push(" AND (edit_expires_ms IS NULL OR edit_expires_ms <= ")
                    .push_bind(millis)
                    .push(")");
            }
            SessionInputMutation::SaveEdit { lease_id, .. }
            | SessionInputMutation::CancelEdit { lease_id } => {
                query
                    .push(" AND edit_id = ")
                    .push_bind(lease_id)
                    .push(" AND edit_expires_ms > ")
                    .push_bind(millis);
            }
            SessionInputMutation::Prioritize => {
                query.push(" AND priority = 0");
            }
        }
        query.push(" RETURNING ").push(COLUMNS);
        let row = query.build().fetch_optional(self.db.pool()).await?;
        if row.is_some() && !secrets.is_empty() {
            leveler_core::register_session_secrets(session_id.as_str(), &secrets);
        }
        row.as_ref().map(decode).transpose()
    }

    /// Request acceleration for a set of scoped pending inputs atomically.
    pub async fn prioritize(
        &self,
        session_id: &SessionId,
        ids: &[&str],
    ) -> Result<Vec<SessionInputRecord>, StorageError> {
        let ids = serde_json::to_string(ids)
            .map_err(|error| StorageError::InvalidData(error.to_string()))?;
        let rows = sqlx::query(&format!(
            "UPDATE session_inputs SET priority = 1 WHERE session_id = ?1
             AND state = 'pending' AND priority = 0 AND id IN (SELECT value FROM json_each(?2))
             RETURNING {COLUMNS}"
        ))
        .bind(session_id.as_str())
        .bind(ids)
        .fetch_all(self.db.pool())
        .await?;
        rows.iter().map(decode).collect()
    }

    /// Settle only the inputs the caller resolved to this execution lineage.
    pub async fn cancel_scoped(
        &self,
        session_id: &SessionId,
        ids: &[&str],
        reason: &str,
        now: Timestamp,
    ) -> Result<Vec<SessionInputRecord>, StorageError> {
        let ids = serde_json::to_string(ids)
            .map_err(|error| StorageError::InvalidData(error.to_string()))?;
        let rows = sqlx::query(&format!(
            "UPDATE session_inputs SET state = 'cancelled', reason = ?3, settled_at = ?4,
             edit_id = NULL, edit_expires_ms = NULL WHERE session_id = ?1
             AND state = 'pending' AND id IN (SELECT value FROM json_each(?2))
             RETURNING {COLUMNS}"
        ))
        .bind(session_id.as_str())
        .bind(ids)
        .bind(reason)
        .bind(now.to_rfc3339())
        .fetch_all(self.db.pool())
        .await?;
        rows.iter().map(decode).collect()
    }

    /// Inputs still awaiting delivery, oldest first.
    pub async fn pending(
        &self,
        session_id: &SessionId,
    ) -> Result<Vec<SessionInputRecord>, StorageError> {
        self.rows_where(
            session_id,
            "SELECT {columns} FROM session_inputs \
             WHERE session_id = ?1 AND state = 'pending' ORDER BY created_at, rowid",
        )
        .await
    }

    /// Every journal-only input (pending, consumed, cancelled), oldest first.
    /// Used by snapshots to project inputs that have no transcript row.
    pub async fn all(
        &self,
        session_id: &SessionId,
    ) -> Result<Vec<SessionInputRecord>, StorageError> {
        self.rows_where(
            session_id,
            "SELECT {columns} FROM session_inputs \
             WHERE session_id = ?1 ORDER BY created_at, rowid",
        )
        .await
    }

    async fn rows_where(
        &self,
        session_id: &SessionId,
        query: &str,
    ) -> Result<Vec<SessionInputRecord>, StorageError> {
        let rows = sqlx::query(&query.replace("{columns}", COLUMNS))
            .bind(session_id.as_str())
            .fetch_all(self.db.pool())
            .await?;
        rows.iter().map(decode).collect()
    }

    /// Settle one pending input as cancelled. Returns `None` when it was
    /// already consumed or cancelled: a late cancel must not rewrite history.
    pub async fn cancel(
        &self,
        session_id: &SessionId,
        id: &str,
        reason: &str,
        now: Timestamp,
    ) -> Result<Option<SessionInputRecord>, StorageError> {
        let changed = sqlx::query(
            "UPDATE session_inputs SET state = 'cancelled', reason = ?3, settled_at = ?4 \
             WHERE session_id = ?1 AND id = ?2 AND state = 'pending'",
        )
        .bind(session_id.as_str())
        .bind(id)
        .bind(reason)
        .bind(now.to_rfc3339())
        .execute(self.db.pool())
        .await?
        .rows_affected();
        if changed == 0 {
            return Ok(None);
        }
        self.load(session_id, id).await
    }

    /// Settle every pending input of a session as cancelled. Returns the
    /// settled rows in admission order so the caller can publish each state.
    pub async fn cancel_pending(
        &self,
        session_id: &SessionId,
        reason: &str,
        now: Timestamp,
    ) -> Result<Vec<SessionInputRecord>, StorageError> {
        let mut tx = self.db.pool().begin_with("BEGIN IMMEDIATE").await?;
        let rows = sqlx::query(&format!(
            "SELECT {COLUMNS} FROM session_inputs \
             WHERE session_id = ?1 AND state = 'pending' ORDER BY created_at, rowid"
        ))
        .bind(session_id.as_str())
        .fetch_all(&mut *tx)
        .await?;
        let mut settled = Vec::with_capacity(rows.len());
        for row in &rows {
            let record = decode(row)?;
            sqlx::query(
                "UPDATE session_inputs SET state = 'cancelled', reason = ?2, settled_at = ?3 \
                 WHERE session_id = ?1 AND id = ?4 AND state = 'pending'",
            )
            .bind(session_id.as_str())
            .bind(reason)
            .bind(now.to_rfc3339())
            .bind(&record.id)
            .execute(&mut *tx)
            .await?;
            settled.push(SessionInputRecord {
                state: SessionInputState::Cancelled,
                reason: reason.to_string(),
                settled_at: Some(now.to_rfc3339()),
                ..record
            });
        }
        tx.commit().await?;
        Ok(settled)
    }

    /// Bind a still-pending input to the turn that will deliver it.
    pub async fn bind_turn(
        &self,
        session_id: &SessionId,
        id: &str,
        turn_id: &TurnId,
    ) -> Result<(), StorageError> {
        sqlx::query(
            "UPDATE session_inputs SET turn_id = ?3 \
             WHERE session_id = ?1 AND id = ?2 AND state = 'pending'",
        )
        .bind(session_id.as_str())
        .bind(id)
        .bind(turn_id.as_str())
        .execute(self.db.pool())
        .await?;
        Ok(())
    }

    /// Mark a pending input consumed after a turn's write-ahead record proves
    /// its message is the turn's initiating message. Used only by resume
    /// reconciliation for a crash between that record and the transcript
    /// projection; the normal steer path consumes atomically with the append.
    pub async fn mark_consumed(
        &self,
        session_id: &SessionId,
        id: &str,
        ordinal: Option<u64>,
        now: Timestamp,
    ) -> Result<(), StorageError> {
        sqlx::query(
            "UPDATE session_inputs SET state = 'consumed', ordinal = ?3, settled_at = ?4 \
             WHERE session_id = ?1 AND id = ?2 AND state = 'pending'",
        )
        .bind(session_id.as_str())
        .bind(id)
        .bind(ordinal.map(|value| value as i64))
        .bind(now.to_rfc3339())
        .execute(self.db.pool())
        .await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session_repo::{SessionRecord, SessionRepository};

    async fn session(db: &Database) -> SessionId {
        let record = SessionRecord::new("/r", "g", "m", leveler_core::now());
        SessionRepository::new(db).create(&record).await.unwrap();
        SessionId::new(record.id)
    }

    /// The ACK boundary is this row: a process death after it must leave the
    /// input recoverable, and a retry must not create a second identity.
    #[tokio::test]
    async fn admission_is_durable_and_idempotent() {
        let db = Database::connect_in_memory().await.unwrap();
        let session_id = session(&db).await;
        let repo = SessionInputRepository::new(&db);

        let first = repo
            .admit(
                "input-1",
                &session_id,
                None,
                "还有管理员管理也没有",
                leveler_core::now(),
            )
            .await
            .unwrap();
        assert_eq!(first.state, SessionInputState::Pending);
        assert_eq!(first.content, "还有管理员管理也没有");

        let retry = repo
            .admit(
                "input-1",
                &session_id,
                None,
                "还有管理员管理也没有",
                leveler_core::now(),
            )
            .await
            .unwrap();
        assert_eq!(retry, first, "a retry with the same id is one input");

        let pending = repo.pending(&session_id).await.unwrap();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].id, "input-1");
    }

    /// Cancel is a terminal settlement, not a delete. A cancelled row must
    /// never be handed to a later turn.
    #[tokio::test]
    async fn cancel_settles_pending_and_never_resurrects() {
        let db = Database::connect_in_memory().await.unwrap();
        let session_id = session(&db).await;
        let repo = SessionInputRepository::new(&db);
        let now = leveler_core::now();
        repo.admit("a", &session_id, None, "first", now)
            .await
            .unwrap();
        repo.admit("b", &session_id, None, "second", now)
            .await
            .unwrap();

        let settled = repo
            .cancel_pending(&session_id, "turn cancelled", now)
            .await
            .unwrap();
        assert_eq!(
            settled.iter().map(|r| r.id.as_str()).collect::<Vec<_>>(),
            vec!["a", "b"]
        );
        assert!(
            settled
                .iter()
                .all(|r| r.state == SessionInputState::Cancelled)
        );
        assert!(repo.pending(&session_id).await.unwrap().is_empty());
        assert_eq!(
            repo.load(&session_id, "a").await.unwrap().unwrap().reason,
            "turn cancelled"
        );

        // A duplicate admission must not reset the terminal state.
        let again = repo
            .admit("a", &session_id, None, "first", now)
            .await
            .unwrap();
        assert_eq!(again.state, SessionInputState::Cancelled);
        assert!(repo.pending(&session_id).await.unwrap().is_empty());
        assert!(
            repo.cancel(&session_id, "a", "late", now)
                .await
                .unwrap()
                .is_none()
        );
    }

    /// F-01: the input journal uses the same value-position sanitizer as the
    /// model-visible tool boundary. Ordinary code survives; a real secret is
    /// hidden and registered for later prose.
    #[tokio::test]
    async fn user_input_keeps_code_and_hides_secret() {
        let db = Database::connect_in_memory().await.unwrap();
        let session_id = session(&db).await;
        let repo = SessionInputRepository::new(&db);
        let code = "let password = config.password;\npub password: String,";
        let admitted = repo
            .admit("code", &session_id, None, code, leveler_core::now())
            .await
            .unwrap();
        assert_eq!(admitted.content, code);

        let secret = "API_PASSWORD=hunter2-abc";
        let admitted = repo
            .admit("secret", &session_id, None, secret, leveler_core::now())
            .await
            .unwrap();
        assert!(
            !admitted.content.contains("hunter2-abc"),
            "{}",
            admitted.content
        );
        assert!(admitted.content.contains("API_PASSWORD"));
        assert!(!leveler_core::SECRET_PLACEHOLDER.is_empty());
        leveler_core::clear_session_secrets(session_id.as_str());
    }

    /// A consumed input keeps the exact transcript ordinal that its message
    /// occupies, so a client can deduplicate a live row against the snapshot.
    #[tokio::test]
    async fn consumed_records_its_transcript_ordinal() {
        let db = Database::connect_in_memory().await.unwrap();
        let session_id = session(&db).await;
        let repo = SessionInputRepository::new(&db);
        let now = leveler_core::now();
        repo.admit("input-1", &session_id, None, "do it", now)
            .await
            .unwrap();

        repo.mark_consumed(&session_id, "input-1", Some(7), now)
            .await
            .unwrap();
        let row = repo.load(&session_id, "input-1").await.unwrap().unwrap();
        assert_eq!(row.state, SessionInputState::Consumed);
        assert_eq!(row.ordinal, Some(7));
        assert!(repo.pending(&session_id).await.unwrap().is_empty());
        assert_eq!(repo.all(&session_id).await.unwrap().len(), 1);
    }
}
