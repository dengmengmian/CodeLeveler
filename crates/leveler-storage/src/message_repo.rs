//! Persistence for resumable session message transcripts.
//!
//! Storage stays domain-agnostic: payloads are opaque JSON strings. The app
//! layer serializes/deserializes the unified `Message` type.

use leveler_core::{SessionId, Timestamp};

use crate::database::{Database, StorageError};

/// Start an IMMEDIATE transaction: the write lock is acquired at BEGIN, so a
/// read-then-write batch cannot hit SQLite's non-waitable upgrade deadlock.
async fn begin_immediate(
    pool: &sqlx::Pool<sqlx::Sqlite>,
) -> Result<sqlx::Transaction<'static, sqlx::Sqlite>, StorageError> {
    Ok(pool.begin_with("BEGIN IMMEDIATE").await?)
}

/// A stored message payload with its position and record time.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TimedMessage {
    /// Append position in the session's message log.
    pub ordinal: u64,
    /// RFC 3339, as written.
    pub created_at: String,
    /// The serialized message.
    pub payload: String,
    /// The accepted-input identity that produced this row, when it was an
    /// admitted append. `None` for every other kind of message.
    pub input_id: Option<String>,
}

/// Read/write access to the `session_messages` table.
pub struct MessageRepository<'a> {
    db: &'a Database,
}

impl<'a> MessageRepository<'a> {
    /// Borrow `db` for the lifetime of this repository handle.
    pub fn new(db: &'a Database) -> Self {
        Self { db }
    }

    /// Append serialized message payloads for a session, continuing the ordinal
    /// sequence. Runs in a transaction so a batch appends atomically.
    pub async fn append(
        &self,
        session_id: &SessionId,
        payloads: &[String],
        now: Timestamp,
    ) -> Result<(), StorageError> {
        if payloads.is_empty() {
            return Ok(());
        }
        let ts = now.to_rfc3339();
        // BEGIN IMMEDIATE: take the write lock upfront. A deferred BEGIN that
        // reads (MAX ordinal) and then upgrades to write deadlocks against a
        // concurrent writer (the engine's event pump) with an immediate
        // `database is locked` that no busy_timeout can wait out.
        let mut tx = begin_immediate(self.db.pool()).await?;

        let next: i64 = sqlx::query_scalar(
            "SELECT COALESCE(MAX(ordinal), -1) + 1 FROM session_messages WHERE session_id = ?1",
        )
        .bind(session_id.as_str())
        .fetch_one(&mut *tx)
        .await?;

        for (offset, payload) in payloads.iter().enumerate() {
            let redacted = crate::redact_json_payload_for_session(
                "session message",
                payload,
                Some(session_id.as_str()),
            )?;
            sqlx::query(
                "INSERT INTO session_messages (session_id, ordinal, payload, created_at) \
                 VALUES (?1, ?2, ?3, ?4)",
            )
            .bind(session_id.as_str())
            .bind(next + offset as i64)
            .bind(&redacted)
            .bind(&ts)
            .execute(&mut *tx)
            .await?;
        }

        tx.commit().await?;
        Ok(())
    }

    /// Like [`MessageRepository::append`], but stamps every row with the
    /// owning turn, so resume can rebuild per-turn transcripts.
    pub async fn append_in_turn(
        &self,
        session_id: &SessionId,
        turn_id: &leveler_core::TurnId,
        payloads: &[String],
        now: Timestamp,
    ) -> Result<(), StorageError> {
        if payloads.is_empty() {
            return Ok(());
        }
        let ts = now.to_rfc3339();
        // BEGIN IMMEDIATE: take the write lock upfront. A deferred BEGIN that
        // reads (MAX ordinal) and then upgrades to write deadlocks against a
        // concurrent writer (the engine's event pump) with an immediate
        // `database is locked` that no busy_timeout can wait out.
        let mut tx = begin_immediate(self.db.pool()).await?;
        let next: i64 = sqlx::query_scalar(
            "SELECT COALESCE(MAX(ordinal), -1) + 1 FROM session_messages WHERE session_id = ?1",
        )
        .bind(session_id.as_str())
        .fetch_one(&mut *tx)
        .await?;
        for (offset, payload) in payloads.iter().enumerate() {
            let redacted = crate::redact_json_payload_for_session(
                "session message",
                payload,
                Some(session_id.as_str()),
            )?;
            sqlx::query(
                "INSERT INTO session_messages (session_id, ordinal, payload, created_at, turn_id) \
                 VALUES (?1, ?2, ?3, ?4, ?5)",
            )
            .bind(session_id.as_str())
            .bind(next + offset as i64)
            .bind(&redacted)
            .bind(&ts)
            .bind(turn_id.as_str())
            .execute(&mut *tx)
            .await?;
        }
        tx.commit().await?;
        Ok(())
    }

    /// Fenced [`Self::append_in_turn`]: the ownership check runs INSIDE the
    /// same immediate transaction as the inserts, so a stale token stores
    /// nothing — no check-then-write window.
    pub async fn append_in_turn_owned(
        &self,
        token: &leveler_core::OwnershipToken,
        session_id: &SessionId,
        turn_id: &leveler_core::TurnId,
        payloads: &[String],
        now: Timestamp,
    ) -> Result<(), crate::OwnershipError> {
        if payloads.is_empty() {
            return Ok(());
        }
        let ts = now.to_rfc3339();
        let mut tx = begin_immediate(self.db.pool())
            .await
            .map_err(crate::OwnershipError::Storage)?;
        let current: Option<(Option<String>, i64)> = sqlx::query_as(
            "SELECT owner_runtime_id, owner_epoch FROM tasks WHERE session_id = ?1 AND id = ?2",
        )
        .bind(session_id.as_str())
        .bind(token.task_id.as_str())
        .fetch_optional(&mut *tx)
        .await
        .map_err(|e| crate::OwnershipError::Storage(e.into()))?;
        let owned = current.as_ref().is_some_and(|(runtime, epoch)| {
            runtime.as_deref() == Some(token.runtime_id.as_str())
                && *epoch == token.owner_epoch.get() as i64
        });
        if !owned {
            drop(tx); // implicit rollback: nothing was written
            return Err(crate::ownership_store::sqlite_stale_error(self.db, token).await);
        }
        let next: i64 = sqlx::query_scalar(
            "SELECT COALESCE(MAX(ordinal), -1) + 1 FROM session_messages WHERE session_id = ?1",
        )
        .bind(session_id.as_str())
        .fetch_one(&mut *tx)
        .await
        .map_err(|e| crate::OwnershipError::Storage(e.into()))?;
        for (offset, payload) in payloads.iter().enumerate() {
            let redacted = crate::redact_json_payload_for_session(
                "session message",
                payload,
                Some(session_id.as_str()),
            )?;
            sqlx::query(
                "INSERT INTO session_messages (session_id, ordinal, payload, created_at, turn_id) \
                 VALUES (?1, ?2, ?3, ?4, ?5)",
            )
            .bind(session_id.as_str())
            .bind(next + offset as i64)
            .bind(&redacted)
            .bind(&ts)
            .bind(turn_id.as_str())
            .execute(&mut *tx)
            .await
            .map_err(|e| crate::OwnershipError::Storage(e.into()))?;
        }
        // Test-only crash point INSIDE the uncommitted batch: a real kill here
        // must leave no partial transcript, proving the batch atomicity that
        // T5 relies on. Only the assistant/tool batch is a target; the user
        // row append must not fire it. Normal runs never reach the kill path.
        if payloads
            .iter()
            .any(|payload| payload.contains("\"role\":\"tool\""))
        {
            leveler_core::test_crash::point("during_transcript_commit");
        }
        tx.commit()
            .await
            .map_err(|e| crate::OwnershipError::Storage(e.into()))?;
        Ok(())
    }

    /// Fenced append of one accepted input's transcript row. The journal
    /// `pending -> consumed` transition and the row insert share one
    /// `BEGIN IMMEDIATE` transaction, so no process death can leave the text
    /// both out of the transcript and out of the pending queue.
    ///
    /// Returns the row's ordinal, or `None` when the input is no longer
    /// pending (already consumed or cancelled), or when its admitted text no
    /// longer matches the journal (an editor changed it after this round read
    /// it). `None` stores nothing.
    pub async fn append_input_in_turn_owned(
        &self,
        token: &leveler_core::OwnershipToken,
        session_id: &SessionId,
        turn_id: &leveler_core::TurnId,
        input_id: &str,
        payload: &str,
        now: Timestamp,
    ) -> Result<Option<u64>, crate::OwnershipError> {
        // The journal is the identity authority: it stores the admitted text,
        // while the transcript stores a redacted projection of it. A secret
        // registered after admission, or a placeholder the admission pass
        // only partially redacted, must not look like "the user edited this
        // input". Compare against the unredacted serialization, then insert
        // the redacted one.
        let redacted = crate::redact_json_payload_for_session(
            "session message",
            payload,
            Some(session_id.as_str()),
        )?;
        let mut tx = begin_immediate(self.db.pool())
            .await
            .map_err(crate::OwnershipError::Storage)?;
        let current: Option<(Option<String>, i64)> = sqlx::query_as(
            "SELECT owner_runtime_id, owner_epoch FROM tasks WHERE session_id = ?1 AND id = ?2",
        )
        .bind(session_id.as_str())
        .bind(token.task_id.as_str())
        .fetch_optional(&mut *tx)
        .await
        .map_err(|e| crate::OwnershipError::Storage(e.into()))?;
        let owned = current.as_ref().is_some_and(|(runtime, epoch)| {
            runtime.as_deref() == Some(token.runtime_id.as_str())
                && *epoch == token.owner_epoch.get() as i64
        });
        if !owned {
            drop(tx);
            return Err(crate::ownership_store::sqlite_stale_error(self.db, token).await);
        }
        let eligible: Option<i64> = sqlx::query_scalar(
            "SELECT 1 FROM session_inputs WHERE session_id = ?1 AND id = ?2 \
             AND state = 'pending' AND (edit_expires_ms IS NULL OR edit_expires_ms <= ?3) \
             AND content = json_extract(?4, '$.content[0].text')",
        )
        .bind(session_id.as_str())
        .bind(input_id)
        .bind(now.timestamp_millis())
        .bind(payload)
        .fetch_optional(&mut *tx)
        .await
        .map_err(|e| crate::OwnershipError::Storage(e.into()))?;
        if eligible.is_none() {
            // An editor may have taken or changed the input after it was read.
            // Only the current unreserved text can cross the context boundary.
            tx.commit()
                .await
                .map_err(|e| crate::OwnershipError::Storage(e.into()))?;
            return Ok(None);
        }
        let next: i64 = sqlx::query_scalar(
            "SELECT COALESCE(MAX(ordinal), -1) + 1 FROM session_messages WHERE session_id = ?1",
        )
        .bind(session_id.as_str())
        .fetch_one(&mut *tx)
        .await
        .map_err(|e| crate::OwnershipError::Storage(e.into()))?;
        sqlx::query(
            "INSERT INTO session_messages \
             (session_id, ordinal, payload, created_at, turn_id, input_id) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        )
        .bind(session_id.as_str())
        .bind(next)
        .bind(&redacted)
        .bind(now.to_rfc3339())
        .bind(turn_id.as_str())
        .bind(input_id)
        .execute(&mut *tx)
        .await
        .map_err(|e| crate::OwnershipError::Storage(e.into()))?;
        let settled = sqlx::query(
            "UPDATE session_inputs \
             SET state = 'consumed', turn_id = ?3, ordinal = ?4, settled_at = ?5, \
                 edit_id = NULL, edit_expires_ms = NULL \
             WHERE session_id = ?1 AND id = ?2 AND state = 'pending'",
        )
        .bind(session_id.as_str())
        .bind(input_id)
        .bind(turn_id.as_str())
        .bind(next)
        .bind(now.to_rfc3339())
        .execute(&mut *tx)
        .await
        .map_err(|e| crate::OwnershipError::Storage(e.into()))?
        .rows_affected();
        if settled != 1 {
            return Err(crate::OwnershipError::Storage(StorageError::InvalidData(
                "input left pending inside its consume transaction".into(),
            )));
        }
        tx.commit()
            .await
            .map_err(|e| crate::OwnershipError::Storage(e.into()))?;
        Ok(Some(next as u64))
    }

    /// Fenced, idempotent recovery projection for one turn's initiating user
    /// message. Identity is `(session_id, turn_id, role=user)`; the content is
    /// deliberately not compared because equal user text can start distinct
    /// turns. `BEGIN IMMEDIATE` serializes the check and insert.
    pub async fn ensure_initiating_message_owned(
        &self,
        token: &leveler_core::OwnershipToken,
        session_id: &SessionId,
        turn_id: &leveler_core::TurnId,
        payload: &str,
        now: Timestamp,
    ) -> Result<bool, crate::OwnershipError> {
        let redacted = crate::redact_json_payload_for_session(
            "session message",
            payload,
            Some(session_id.as_str()),
        )?;
        let mut tx = begin_immediate(self.db.pool())
            .await
            .map_err(crate::OwnershipError::Storage)?;
        let current: Option<(Option<String>, i64)> = sqlx::query_as(
            "SELECT owner_runtime_id, owner_epoch FROM tasks WHERE session_id = ?1 AND id = ?2",
        )
        .bind(session_id.as_str())
        .bind(token.task_id.as_str())
        .fetch_optional(&mut *tx)
        .await
        .map_err(|e| crate::OwnershipError::Storage(e.into()))?;
        let owned = current.as_ref().is_some_and(|(runtime, epoch)| {
            runtime.as_deref() == Some(token.runtime_id.as_str())
                && *epoch == token.owner_epoch.get() as i64
        });
        if !owned {
            drop(tx);
            return Err(crate::ownership_store::sqlite_stale_error(self.db, token).await);
        }
        let exists: bool = sqlx::query_scalar(
            "SELECT EXISTS(SELECT 1 FROM session_messages \
             WHERE session_id = ?1 AND turn_id = ?2 \
             AND json_extract(payload, '$.role') = 'user')",
        )
        .bind(session_id.as_str())
        .bind(turn_id.as_str())
        .fetch_one(&mut *tx)
        .await
        .map_err(|e| crate::OwnershipError::Storage(e.into()))?;
        if exists {
            tx.commit()
                .await
                .map_err(|e| crate::OwnershipError::Storage(e.into()))?;
            return Ok(false);
        }
        // F-24: a `/compact` epoch cut removes the transcript row but must not
        // make the reaper restore it. The input journal is the durable proof
        // that this turn's initiating input was already admitted AND projected
        // (the consume transaction inserts the row and marks the journal in one
        // commit). If it is consumed, a missing row can only mean an epoch cut,
        // so re-inserting would resurrect history the user compacted and break
        // the summary-only epoch. Legacy turns with no journal row keep the
        // old repair behavior.
        let consumed: bool = sqlx::query_scalar(
            "SELECT EXISTS(SELECT 1 FROM session_inputs \
             WHERE session_id = ?1 AND turn_id = ?2 AND state = 'consumed')",
        )
        .bind(session_id.as_str())
        .bind(turn_id.as_str())
        .fetch_one(&mut *tx)
        .await
        .map_err(|e| crate::OwnershipError::Storage(e.into()))?;
        if consumed {
            tx.commit()
                .await
                .map_err(|e| crate::OwnershipError::Storage(e.into()))?;
            return Ok(false);
        }
        let next: i64 = sqlx::query_scalar(
            "SELECT COALESCE(MAX(ordinal), -1) + 1 FROM session_messages WHERE session_id = ?1",
        )
        .bind(session_id.as_str())
        .fetch_one(&mut *tx)
        .await
        .map_err(|e| crate::OwnershipError::Storage(e.into()))?;
        sqlx::query(
            "INSERT INTO session_messages (session_id, ordinal, payload, created_at, turn_id) \
             VALUES (?1, ?2, ?3, ?4, ?5)",
        )
        .bind(session_id.as_str())
        .bind(next)
        .bind(redacted)
        .bind(now.to_rfc3339())
        .bind(turn_id.as_str())
        .execute(&mut *tx)
        .await
        .map_err(|e| crate::OwnershipError::Storage(e.into()))?;
        tx.commit()
            .await
            .map_err(|e| crate::OwnershipError::Storage(e.into()))?;
        Ok(true)
    }

    /// Load the payloads of one turn, in order.
    pub async fn load_for_turn(
        &self,
        session_id: &SessionId,
        turn_id: &leveler_core::TurnId,
    ) -> Result<Vec<String>, StorageError> {
        let rows: Vec<(String,)> = sqlx::query_as(
            "SELECT payload FROM session_messages \
             WHERE session_id = ?1 AND turn_id = ?2 ORDER BY ordinal ASC",
        )
        .bind(session_id.as_str())
        .bind(turn_id.as_str())
        .fetch_all(self.db.pool())
        .await?;
        Ok(rows.into_iter().map(|(p,)| p).collect())
    }

    /// Number of persisted messages for a session (the next append ordinal).
    pub async fn count(&self, session_id: &SessionId) -> Result<u64, StorageError> {
        let (count,): (i64,) =
            sqlx::query_as("SELECT COUNT(*) FROM session_messages WHERE session_id = ?1")
                .bind(session_id.as_str())
                .fetch_one(self.db.pool())
                .await?;
        Ok(count.max(0) as u64)
    }

    /// Total bytes of a session's stored payloads, without reading them.
    pub async fn total_bytes(&self, session_id: &SessionId) -> Result<u64, StorageError> {
        let (total,): (i64,) = sqlx::query_as(
            "SELECT COALESCE(SUM(LENGTH(payload)), 0) FROM session_messages \
             WHERE session_id = ?1",
        )
        .bind(session_id.as_str())
        .fetch_one(self.db.pool())
        .await?;
        Ok(total.max(0) as u64)
    }

    /// Payloads at ordinal `from` and later, in order. Ordinals are dense and
    /// assigned on append, so this is the transcript tail after a watermark.
    pub async fn load_from(
        &self,
        session_id: &SessionId,
        from: u64,
    ) -> Result<Vec<String>, StorageError> {
        let rows: Vec<(String,)> = sqlx::query_as(
            "SELECT payload FROM session_messages \
             WHERE session_id = ?1 AND ordinal >= ?2 ORDER BY ordinal ASC",
        )
        .bind(session_id.as_str())
        .bind(from.min(i64::MAX as u64) as i64)
        .fetch_all(self.db.pool())
        .await?;
        Ok(rows.into_iter().map(|(p,)| p).collect())
    }

    /// All of a session's payloads in order, each with its ordinal and record
    /// time, for a history view that places messages among events.
    pub async fn load_timed(
        &self,
        session_id: &SessionId,
    ) -> Result<Vec<TimedMessage>, StorageError> {
        self.load_timed_since(session_id, None).await
    }

    /// A bounded page of messages after `after_ordinal`, oldest first.
    ///
    /// The reviewer reads a session on demand; it must be able to ask for the
    /// next page without loading the whole transcript first.
    pub async fn page(
        &self,
        session_id: &SessionId,
        after_ordinal: i64,
        limit: usize,
    ) -> Result<Vec<TimedMessage>, StorageError> {
        let rows: Vec<(i64, String, String, Option<String>)> = sqlx::query_as(
            "SELECT ordinal, created_at, payload, input_id FROM session_messages \
             WHERE session_id = ?1 AND ordinal > ?2 ORDER BY ordinal ASC LIMIT ?3",
        )
        .bind(session_id.as_str())
        .bind(after_ordinal)
        .bind(i64::try_from(limit).unwrap_or(i64::MAX))
        .fetch_all(self.db.pool())
        .await?;
        Ok(rows
            .into_iter()
            .map(|(ordinal, created_at, payload, input_id)| TimedMessage {
                ordinal: ordinal.max(0) as u64,
                created_at,
                payload,
                input_id,
            })
            .collect())
    }

    /// Bounded literal text search over one session's stored messages.
    ///
    /// Same contract as the event search: `instr` is a literal substring
    /// test, so user text is never interpreted as a SQL pattern.
    pub async fn search(
        &self,
        session_id: &SessionId,
        query: &str,
        limit: usize,
    ) -> Result<Vec<TimedMessage>, StorageError> {
        let rows: Vec<(i64, String, String, Option<String>)> = sqlx::query_as(
            "SELECT ordinal, created_at, payload, input_id FROM session_messages              WHERE session_id = ?1 AND instr(lower(payload), lower(?2)) > 0              ORDER BY ordinal ASC LIMIT ?3",
        )
        .bind(session_id.as_str())
        .bind(query)
        .bind(i64::try_from(limit).unwrap_or(i64::MAX))
        .fetch_all(self.db.pool())
        .await?;
        Ok(rows
            .into_iter()
            .map(|(ordinal, created_at, payload, input_id)| TimedMessage {
                ordinal: ordinal.max(0) as u64,
                created_at,
                payload,
                input_id,
            })
            .collect())
    }

    /// A history tail, preserving rows whose timestamp cannot be interpreted.
    pub async fn load_timed_since(
        &self,
        session_id: &SessionId,
        since: Option<&str>,
    ) -> Result<Vec<TimedMessage>, StorageError> {
        let rows: Vec<(i64, String, String, Option<String>)> = sqlx::query_as(
            "SELECT ordinal, created_at, payload, input_id FROM session_messages \
             WHERE session_id = ?1 AND (?2 IS NULL OR julianday(created_at) >= julianday(?2) OR julianday(created_at) IS NULL) ORDER BY ordinal ASC",
        )
        .bind(session_id.as_str())
        .bind(since)
        .fetch_all(self.db.pool())
        .await?;
        Ok(rows
            .into_iter()
            .map(|(ordinal, created_at, payload, input_id)| TimedMessage {
                ordinal: ordinal.max(0) as u64,
                created_at,
                payload,
                input_id,
            })
            .collect())
    }

    /// Load all message payloads for a session, in order.
    pub async fn load(&self, session_id: &SessionId) -> Result<Vec<String>, StorageError> {
        let rows: Vec<(String,)> = sqlx::query_as(
            "SELECT payload FROM session_messages WHERE session_id = ?1 ORDER BY ordinal ASC",
        )
        .bind(session_id.as_str())
        .fetch_all(self.db.pool())
        .await?;
        Ok(rows.into_iter().map(|(p,)| p).collect())
    }

    /// Every durable payload that may reference a media-store object, across
    /// all sessions.
    ///
    /// This is a conservative superset built from the places an image can be
    /// persisted: transcript rows, the write-ahead turn row (which survives
    /// `/compact` and `/clear`), tool-image events (content hashes), and event
    /// payloads that inline a message such as a context snapshot. The app layer
    /// parses each payload and derives the hashes; storage stays the one owner
    /// of SQL access.
    ///
    /// The filter is deliberately textual and over-inclusive: a missed payload
    /// could make a live image look unreferenced and get it deleted, while a
    /// false positive only costs a parse. A payload that cannot be parsed must
    /// make the caller skip the sweep, never treat the object as unowned.
    pub async fn media_reference_payloads(&self) -> Result<Vec<String>, StorageError> {
        let rows: Vec<(String,)> = sqlx::query_as(
            "SELECT payload FROM session_messages \
               WHERE payload LIKE '%\"base64\"%' AND payload LIKE '%\"image\"%' \
             UNION ALL \
             SELECT payload FROM turns \
               WHERE payload LIKE '%\"base64\"%' AND payload LIKE '%\"image\"%' \
             UNION ALL \
             SELECT payload FROM events \
               WHERE type = 'tool_call_images' \
                  OR (payload LIKE '%\"base64\"%' AND payload LIKE '%\"image\"%')",
        )
        .fetch_all(self.db.pool())
        .await?;
        Ok(rows.into_iter().map(|(payload,)| payload).collect())
    }

    /// The payloads of this session's review-settlement notices, in order.
    ///
    /// The notice row is the durable record that a settled review's result was
    /// delivered to the conversation. The review row cannot answer that: a
    /// review written before the result-message mechanism has the same
    /// terminal fields, but no notice. The filter reads the structured origin
    /// this runtime wrote, never prose in the body.
    pub async fn review_settled_notice_payloads(
        &self,
        session_id: &SessionId,
    ) -> Result<Vec<String>, StorageError> {
        let rows: Vec<(String,)> = sqlx::query_as(
            "SELECT payload FROM session_messages \
             WHERE session_id = ?1 \
               AND json_extract(payload, '$.origin.kind') = 'runtime_notice' \
               AND json_extract(payload, '$.origin.notice') = 'review_settled' \
             ORDER BY ordinal ASC",
        )
        .bind(session_id.as_str())
        .fetch_all(self.db.pool())
        .await?;
        Ok(rows.into_iter().map(|(p,)| p).collect())
    }

    /// Load all message payloads with the accepted-input identity that
    /// produced each one, in order. `None` for messages that were not an
    /// admitted append (seed submits, assistant turns, tool results).
    pub async fn load_with_input_ids(
        &self,
        session_id: &SessionId,
    ) -> Result<Vec<(String, Option<String>)>, StorageError> {
        let rows: Vec<(String, Option<String>)> = sqlx::query_as(
            "SELECT payload, input_id FROM session_messages \
             WHERE session_id = ?1 ORDER BY ordinal ASC",
        )
        .bind(session_id.as_str())
        .fetch_all(self.db.pool())
        .await?;
        Ok(rows)
    }

    /// The first user-role message text of every session, keyed by session id
    /// (display-title source for sessions whose goal is a placeholder).
    ///
    /// The one deliberate exception to "payloads are opaque JSON": the query
    /// peeks at `$.role` so it can pick one row per session inside SQLite
    /// instead of loading every transcript. Uses SQLite's documented bare-
    /// column-with-MIN() guarantee to take each session's earliest user row.
    pub async fn first_user_texts(
        &self,
    ) -> Result<std::collections::HashMap<String, String>, StorageError> {
        let rows: Vec<(String, String, i64)> = sqlx::query_as(
            "SELECT session_id, payload, MIN(ordinal) FROM session_messages \
             WHERE json_extract(payload, '$.role') = 'user' \
             GROUP BY session_id",
        )
        .fetch_all(self.db.pool())
        .await?;
        let mut out = std::collections::HashMap::new();
        for (session_id, payload, _) in rows {
            let Ok(value) = serde_json::from_str::<serde_json::Value>(&payload) else {
                continue;
            };
            let text = value
                .get("content")
                .and_then(|c| c.as_array())
                .and_then(|parts| {
                    parts.iter().find_map(|p| {
                        (p.get("type")?.as_str()? == "text")
                            .then(|| p.get("text")?.as_str())
                            .flatten()
                    })
                })
                .unwrap_or_default();
            if !text.trim().is_empty() {
                out.insert(session_id, text.to_string());
            }
        }
        Ok(out)
    }

    /// Delete all messages at or after `keep` (0-based ordinal), truncating the
    /// transcript back to its first `keep` messages. Used by conversation
    /// rollback / checkpoint restore.
    pub async fn truncate_after(
        &self,
        session_id: &SessionId,
        keep: usize,
    ) -> Result<(), StorageError> {
        sqlx::query("DELETE FROM session_messages WHERE session_id = ?1 AND ordinal >= ?2")
            .bind(session_id.as_str())
            .bind(keep as i64)
            .execute(self.db.pool())
            .await?;
        Ok(())
    }

    /// Atomically replace the entire session transcript with `payloads`.
    /// Either the full new history is committed, or the prior history is left
    /// intact (no truncate-then-fail gap).
    pub async fn replace_all(
        &self,
        session_id: &SessionId,
        payloads: &[String],
        now: Timestamp,
    ) -> Result<(), StorageError> {
        let ts = now.to_rfc3339();
        let mut tx = begin_immediate(self.db.pool()).await?;
        sqlx::query("DELETE FROM session_messages WHERE session_id = ?1")
            .bind(session_id.as_str())
            .execute(&mut *tx)
            .await?;
        for (ordinal, payload) in payloads.iter().enumerate() {
            let redacted = crate::redact_json_payload_for_session(
                "session message",
                payload,
                Some(session_id.as_str()),
            )?;
            sqlx::query(
                "INSERT INTO session_messages (session_id, ordinal, payload, created_at) \
                 VALUES (?1, ?2, ?3, ?4)",
            )
            .bind(session_id.as_str())
            .bind(ordinal as i64)
            .bind(&redacted)
            .bind(&ts)
            .execute(&mut *tx)
            .await?;
        }
        tx.commit().await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session_repo::{SessionRecord, SessionRepository};

    #[tokio::test]
    async fn append_and_load_preserves_order() {
        let db = Database::connect_in_memory().await.unwrap();
        let session = SessionRecord::new("/r", "g", "m", leveler_core::now());
        SessionRepository::new(&db).create(&session).await.unwrap();
        let id = SessionId::new(session.id.clone());

        let repo = MessageRepository::new(&db);
        repo.append(
            &id,
            &[r#""a""#.into(), r#""b""#.into()],
            leveler_core::now(),
        )
        .await
        .unwrap();
        repo.append(&id, &[r#""c""#.into()], leveler_core::now())
            .await
            .unwrap();

        assert_eq!(
            repo.load(&id).await.unwrap(),
            vec![r#""a""#, r#""b""#, r#""c""#]
        );
    }

    /// The delivery record is the structured review settlement notice, not a
    /// review row or a phrase in some other row.
    #[tokio::test]
    async fn only_review_settlement_notices_are_selected() {
        let db = Database::connect_in_memory().await.unwrap();
        let session = SessionRecord::new("/r", "g", "m", leveler_core::now());
        SessionRepository::new(&db).create(&session).await.unwrap();
        let id = SessionId::new(session.id.clone());
        let repo = MessageRepository::new(&db);
        let plain = r#"{"role":"user","content":[{"type":"text","text":"hello"}]}"#;
        let notice = |kind: &str| {
            serde_json::json!({
                "role": "user",
                "content": [{"type": "text", "text": format!("## Review {kind}\n\n- review_id: review-1")}],
                "origin": {"kind": "runtime_notice", "notice": format!("review_{kind}")},
            })
            .to_string()
        };
        let started = notice("started");
        let settled = notice("settled");
        let protocol_repair = r#"{"role":"user","content":[{"type":"text","text":"repair"}],"origin":{"kind":"protocol_repair","repair":"missing_tool_result"}}"#;
        repo.append(
            &id,
            &[
                plain.to_string(),
                started.to_string(),
                protocol_repair.to_string(),
                settled.to_string(),
            ],
            leveler_core::now(),
        )
        .await
        .unwrap();

        let payloads = repo.review_settled_notice_payloads(&id).await.unwrap();
        assert_eq!(payloads.len(), 1, "{payloads:?}");
        assert_eq!(payloads[0], settled);
    }

    /// A history replay interleaves messages with events by record time.
    #[tokio::test]
    async fn load_timed_carries_each_payloads_record_time() {
        let db = Database::connect_in_memory().await.unwrap();
        let session = SessionRecord::new("/r", "g", "m", leveler_core::now());
        SessionRepository::new(&db).create(&session).await.unwrap();
        let id = SessionId::new(session.id.clone());
        let repo = MessageRepository::new(&db);
        let first = leveler_core::now();
        let later = first + chrono::Duration::seconds(5);
        repo.append(&id, &[r#""a""#.into()], first).await.unwrap();
        repo.append(&id, &[r#""b""#.into()], later).await.unwrap();

        let rows = repo.load_timed(&id).await.unwrap();
        assert_eq!(
            rows.iter().map(|r| r.payload.as_str()).collect::<Vec<_>>(),
            vec![r#""a""#, r#""b""#]
        );
        assert_eq!(rows[0].created_at, first.to_rfc3339());
        assert_eq!(rows[1].created_at, later.to_rfc3339());
        assert_eq!((rows[0].ordinal, rows[1].ordinal), (0, 1));
    }

    #[tokio::test]
    async fn a_message_page_is_bounded_and_continues_after_its_last_ordinal() {
        let db = Database::connect_in_memory().await.unwrap();
        let session = SessionRecord::new("/r", "g", "m", leveler_core::now());
        SessionRepository::new(&db).create(&session).await.unwrap();
        let id = SessionId::new(session.id);
        let repo = MessageRepository::new(&db);
        repo.append(
            &id,
            &[
                r#"{"n":1}"#.into(),
                r#"{"n":2}"#.into(),
                r#"{"n":3}"#.into(),
            ],
            leveler_core::now(),
        )
        .await
        .unwrap();
        let first = repo.page(&id, -1, 2).await.unwrap();
        assert_eq!(first.len(), 2);
        assert_eq!(first[1].payload, r#"{"n":2}"#);
        let rest = repo.page(&id, first[1].ordinal as i64, 2).await.unwrap();
        assert_eq!(rest.len(), 1);
        assert_eq!(rest[0].payload, r#"{"n":3}"#);
    }

    #[tokio::test]
    async fn truncate_after_rolls_back_the_transcript() {
        let db = Database::connect_in_memory().await.unwrap();
        let session = SessionRecord::new("/r", "g", "m", leveler_core::now());
        SessionRepository::new(&db).create(&session).await.unwrap();
        let id = SessionId::new(session.id.clone());

        let repo = MessageRepository::new(&db);
        repo.append(
            &id,
            &[
                r#""a""#.into(),
                r#""b""#.into(),
                r#""c""#.into(),
                r#""d""#.into(),
            ],
            leveler_core::now(),
        )
        .await
        .unwrap();

        repo.truncate_after(&id, 2).await.unwrap();
        assert_eq!(repo.load(&id).await.unwrap(), vec![r#""a""#, r#""b""#]);
    }

    #[tokio::test]
    async fn append_in_turn_stamps_turn_ownership() {
        let db = Database::connect_in_memory().await.unwrap();
        let record = crate::SessionRecord::new("/r", "g", "m", leveler_core::now());
        crate::SessionRepository::new(&db)
            .create(&record)
            .await
            .unwrap();
        let session = SessionId::new(record.id);
        let turn_a = crate::TurnRepository::new(&db)
            .start(&session, "user", None, leveler_core::now())
            .await
            .unwrap();
        let turn_b = crate::TurnRepository::new(&db)
            .start(&session, "repair", None, leveler_core::now())
            .await
            .unwrap();

        let repo = MessageRepository::new(&db);
        let a_id = leveler_core::TurnId::new(turn_a.id);
        let b_id = leveler_core::TurnId::new(turn_b.id);
        repo.append_in_turn(
            &session,
            &a_id,
            &[r#""m1""#.into(), r#""m2""#.into()],
            leveler_core::now(),
        )
        .await
        .unwrap();
        repo.append_in_turn(&session, &b_id, &[r#""m3""#.into()], leveler_core::now())
            .await
            .unwrap();

        // Whole-session ordering is preserved across turns…
        assert_eq!(
            repo.load(&session).await.unwrap(),
            vec![r#""m1""#, r#""m2""#, r#""m3""#]
        );
        // …and each turn owns exactly its own messages.
        assert_eq!(
            repo.load_for_turn(&session, &a_id).await.unwrap(),
            vec![r#""m1""#, r#""m2""#]
        );
        assert_eq!(
            repo.load_for_turn(&session, &b_id).await.unwrap(),
            vec![r#""m3""#]
        );
    }

    #[tokio::test]
    async fn replace_all_is_atomic_success_path() {
        let db = Database::connect_in_memory().await.unwrap();
        let session = SessionRecord::new("/r", "g", "m", leveler_core::now());
        SessionRepository::new(&db).create(&session).await.unwrap();
        let id = SessionId::new(session.id.clone());
        let repo = MessageRepository::new(&db);
        repo.append(
            &id,
            &[r#""a""#.into(), r#""b""#.into(), r#""c""#.into()],
            leveler_core::now(),
        )
        .await
        .unwrap();
        repo.replace_all(&id, &[r#""summary-only""#.into()], leveler_core::now())
            .await
            .unwrap();
        assert_eq!(repo.load(&id).await.unwrap(), vec![r#""summary-only""#]);
    }

    #[tokio::test]
    async fn first_user_texts_picks_each_sessions_earliest_user_message() {
        let db = Database::connect_in_memory().await.unwrap();
        let repo_s = SessionRepository::new(&db);
        let a = SessionRecord::new("/r", "interactive session", "m", leveler_core::now());
        let b = SessionRecord::new("/r", "interactive session", "m", leveler_core::now());
        repo_s.create(&a).await.unwrap();
        repo_s.create(&b).await.unwrap();
        let a_id = SessionId::new(a.id.clone());
        let b_id = SessionId::new(b.id.clone());

        let repo = MessageRepository::new(&db);
        repo.append(
            &a_id,
            &[
                r#"{"role":"system","content":[{"type":"text","text":"rules"}]}"#.into(),
                r#"{"role":"user","content":[{"type":"text","text":"帮我修复登录"}]}"#.into(),
                r#"{"role":"user","content":[{"type":"text","text":"第二条"}]}"#.into(),
            ],
            leveler_core::now(),
        )
        .await
        .unwrap();
        // Session b: first user message is image-first; the text part wins.
        repo.append(
            &b_id,
            &[
                r#"{"role":"user","content":[{"type":"image","source":{}},{"type":"text","text":"看下这张图"}]}"#.into(),
            ],
            leveler_core::now(),
        )
        .await
        .unwrap();

        let texts = repo.first_user_texts().await.unwrap();
        assert_eq!(
            texts.get(a.id.as_str()).map(String::as_str),
            Some("帮我修复登录")
        );
        assert_eq!(
            texts.get(b.id.as_str()).map(String::as_str),
            Some("看下这张图")
        );
    }

    #[tokio::test]
    async fn append_input_consumes_the_journal_row_atomically() {
        use crate::{SessionInputRepository, SessionInputState};

        let db = Database::connect_in_memory().await.unwrap();
        let record = SessionRecord::new("/r", "g", "m", leveler_core::now());
        SessionRepository::new(&db).create(&record).await.unwrap();
        let session = SessionId::new(record.id.clone());
        let task = crate::TaskStore::ensure_for_session(&db, &session, leveler_core::now())
            .await
            .unwrap();
        let token = crate::OwnershipStore::acquire(
            &db,
            &task,
            &leveler_core::RuntimeId::new("rt"),
            &leveler_core::BootId::new("boot"),
            leveler_core::OwnerEpoch::UNOWNED,
        )
        .await
        .unwrap();
        let turn = crate::TurnRepository::new(&db)
            .start(&session, "chat", None, leveler_core::now())
            .await
            .unwrap();
        let turn_id = leveler_core::TurnId::new(turn.id);
        let repo = MessageRepository::new(&db);
        let inputs = SessionInputRepository::new(&db);
        inputs
            .admit(
                "input-1",
                &session,
                Some(&turn_id),
                "还有管理员管理也没有",
                leveler_core::now(),
            )
            .await
            .unwrap();

        let stale = repo
            .append_input_in_turn_owned(
                &token,
                &session,
                &turn_id,
                "input-1",
                r#"{"role":"user","content":[{"type":"text","text":"a stale draft"}]}"#,
                leveler_core::now(),
            )
            .await
            .unwrap();
        assert_eq!(
            stale, None,
            "only the journal's current content can be committed"
        );
        assert!(repo.load(&session).await.unwrap().is_empty());

        let now = leveler_core::now();
        let reserved = inputs
            .mutate(
                &session,
                "input-1",
                0,
                crate::SessionInputMutation::BeginEdit {
                    lease_id: "lease-a",
                    expires_ms: now.timestamp_millis() + 1_000,
                },
                now,
            )
            .await
            .unwrap()
            .unwrap();
        assert_eq!(reserved.revision, 1);
        let original =
            r#"{"role":"user","content":[{"type":"text","text":"还有管理员管理也没有"}]}"#;
        let during_edit = repo
            .append_input_in_turn_owned(&token, &session, &turn_id, "input-1", original, now)
            .await
            .unwrap();
        assert_eq!(during_edit, None);
        let after_expiry = now + chrono::Duration::seconds(2);
        let late_save = inputs
            .mutate(
                &session,
                "input-1",
                1,
                crate::SessionInputMutation::SaveEdit {
                    lease_id: "lease-a",
                    content: "late text",
                },
                after_expiry,
            )
            .await
            .unwrap();
        assert!(late_save.is_none());

        let ordinal = repo
            .append_input_in_turn_owned(
                &token,
                &session,
                &turn_id,
                "input-1",
                r#"{"role":"user","content":[{"type":"text","text":"还有管理员管理也没有"}]}"#,
                after_expiry,
            )
            .await
            .unwrap();
        assert_eq!(ordinal, Some(0));
        assert_eq!(repo.load(&session).await.unwrap().len(), 1);
        let row = inputs.load(&session, "input-1").await.unwrap().unwrap();
        assert_eq!(row.state, SessionInputState::Consumed);
        assert_eq!(row.ordinal, Some(0));

        // A replay after the transition stores nothing: the input is no
        // longer pending, so this is the exactly-once boundary.
        let replay = repo
            .append_input_in_turn_owned(
                &token,
                &session,
                &turn_id,
                "input-1",
                r#"{"role":"user","content":[{"type":"text","text":"还有管理员管理也没有"}]}"#,
                leveler_core::now(),
            )
            .await
            .unwrap();
        assert_eq!(replay, None);
        assert_eq!(repo.load(&session).await.unwrap().len(), 1);
    }

    /// A steer whose text contains a value already registered for the session
    /// must still be delivered: redaction is a persistence projection, not an
    /// input-identity change. The journal stores the admitted text; the append
    /// transaction must compare against that text before it rewrites the
    /// payload for storage.
    #[tokio::test]
    async fn append_input_delivers_a_registered_secret_redacted() {
        use crate::{SessionInputRepository, SessionInputState};

        let db = Database::connect_in_memory().await.unwrap();
        let record = SessionRecord::new("/r", "g", "m", leveler_core::now());
        SessionRepository::new(&db).create(&record).await.unwrap();
        let session = SessionId::new(record.id.clone());
        let task = crate::TaskStore::ensure_for_session(&db, &session, leveler_core::now())
            .await
            .unwrap();
        let token = crate::OwnershipStore::acquire(
            &db,
            &task,
            &leveler_core::RuntimeId::new("rt-secret"),
            &leveler_core::BootId::new("boot-secret"),
            leveler_core::OwnerEpoch::UNOWNED,
        )
        .await
        .unwrap();
        let turn = crate::TurnRepository::new(&db)
            .start(&session, "chat", None, leveler_core::now())
            .await
            .unwrap();
        let turn_id = leveler_core::TurnId::new(turn.id);
        let repo = MessageRepository::new(&db);
        let inputs = SessionInputRepository::new(&db);
        let secret = "canary-7f3a9c2e5b8d4f6a1c0e8b7d";
        leveler_core::register_session_secrets(
            session.as_str(),
            &[leveler_core::DetectedSecret {
                value: secret.to_string(),
                key: "token".to_string(),
            }],
        );
        let admitted = inputs
            .admit(
                "input-secret",
                &session,
                Some(&turn_id),
                &format!("请用 {secret} 继续，不要换别的方式"),
                leveler_core::now(),
            )
            .await
            .unwrap();
        assert_eq!(
            admitted.content,
            format!("请用 {secret} 继续，不要换别的方式")
        );

        let payload = serde_json::json!({
            "role": "user",
            "content": [{"type": "text", "text": admitted.content}],
            "origin": {"kind": "user_input"}
        })
        .to_string();
        let ordinal = repo
            .append_input_in_turn_owned(
                &token,
                &session,
                &turn_id,
                "input-secret",
                &payload,
                leveler_core::now(),
            )
            .await
            .unwrap();
        assert_eq!(
            ordinal,
            Some(0),
            "a registered secret must not make the accepted steer undeliverable"
        );
        let stored = repo.load(&session).await.unwrap();
        assert_eq!(stored.len(), 1);
        assert!(
            !stored[0].contains(secret),
            "the transcript payload must be projected without the secret: {}",
            stored[0]
        );
        let row = inputs
            .load(&session, "input-secret")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(row.state, SessionInputState::Consumed);
        assert_eq!(row.ordinal, Some(0));
    }

    /// A document can mix a real credential in its header with documentation
    /// placeholders in prose. The admission pass redacts the real value; the
    /// placeholder must survive, and the accepted steer must still be
    /// deliverable.
    #[tokio::test]
    async fn append_input_delivers_a_partially_sanitized_placeholder() {
        use crate::SessionInputRepository;

        let db = Database::connect_in_memory().await.unwrap();
        let record = SessionRecord::new("/r", "g", "m", leveler_core::now());
        SessionRepository::new(&db).create(&record).await.unwrap();
        let session = SessionId::new(record.id.clone());
        let task = crate::TaskStore::ensure_for_session(&db, &session, leveler_core::now())
            .await
            .unwrap();
        let token = crate::OwnershipStore::acquire(
            &db,
            &task,
            &leveler_core::RuntimeId::new("rt-placeholder"),
            &leveler_core::BootId::new("boot-placeholder"),
            leveler_core::OwnerEpoch::UNOWNED,
        )
        .await
        .unwrap();
        let turn = crate::TurnRepository::new(&db)
            .start(&session, "chat", None, leveler_core::now())
            .await
            .unwrap();
        let turn_id = leveler_core::TurnId::new(turn.id);
        let repo = MessageRepository::new(&db);
        let inputs = SessionInputRepository::new(&db);
        let canary = "canary-7f3a9c2e5b8d4f6a1c0e8b7d";
        let admitted = inputs
            .admit(
                "input-placeholder",
                &session,
                Some(&turn_id),
                &format!(
                    "请求头：Authorization: Bearer {canary}\n请将 <YOUR_API_TOKEN> 替换为实际 Token"
                ),
                leveler_core::now(),
            )
            .await
            .unwrap();
        assert!(
            admitted.content.contains("[REDACTED]"),
            "the admission pass redacts the real header value: {}",
            admitted.content
        );
        assert!(
            admitted.content.contains("<YOUR_API_TOKEN>"),
            "the prose placeholder remains: {}",
            admitted.content
        );

        let payload = serde_json::json!({
            "role": "user",
            "content": [{"type": "text", "text": admitted.content}],
            "origin": {"kind": "user_input"}
        })
        .to_string();
        let ordinal = repo
            .append_input_in_turn_owned(
                &token,
                &session,
                &turn_id,
                "input-placeholder",
                &payload,
                leveler_core::now(),
            )
            .await
            .unwrap();
        assert_eq!(
            ordinal,
            Some(0),
            "a partially sanitized document must not make the steer undeliverable"
        );
        let stored = repo.load(&session).await.unwrap();
        assert_eq!(stored.len(), 1);
        assert!(!stored[0].contains(canary), "{}", stored[0]);
        assert!(stored[0].contains("<YOUR_API_TOKEN>"), "{}", stored[0]);
    }

    #[tokio::test]
    async fn cancelled_input_never_reaches_the_transcript() {
        use crate::{SessionInputRepository, SessionInputState};

        let db = Database::connect_in_memory().await.unwrap();
        let record = SessionRecord::new("/r", "g", "m", leveler_core::now());
        SessionRepository::new(&db).create(&record).await.unwrap();
        let session = SessionId::new(record.id.clone());
        let task = crate::TaskStore::ensure_for_session(&db, &session, leveler_core::now())
            .await
            .unwrap();
        let token = crate::OwnershipStore::acquire(
            &db,
            &task,
            &leveler_core::RuntimeId::new("rt"),
            &leveler_core::BootId::new("boot"),
            leveler_core::OwnerEpoch::UNOWNED,
        )
        .await
        .unwrap();
        let turn = crate::TurnRepository::new(&db)
            .start(&session, "chat", None, leveler_core::now())
            .await
            .unwrap();
        let turn_id = leveler_core::TurnId::new(turn.id);
        let repo = MessageRepository::new(&db);
        let inputs = SessionInputRepository::new(&db);
        inputs
            .admit(
                "input-1",
                &session,
                Some(&turn_id),
                "cancel me",
                leveler_core::now(),
            )
            .await
            .unwrap();
        inputs
            .cancel_pending(&session, "turn cancelled", leveler_core::now())
            .await
            .unwrap();

        let ordinal = repo
            .append_input_in_turn_owned(
                &token,
                &session,
                &turn_id,
                "input-1",
                r#"{"role":"user","content":[{"type":"text","text":"cancel me"}]}"#,
                leveler_core::now(),
            )
            .await
            .unwrap();
        assert_eq!(ordinal, None);
        assert!(repo.load(&session).await.unwrap().is_empty());
        assert_eq!(
            inputs
                .load(&session, "input-1")
                .await
                .unwrap()
                .unwrap()
                .state,
            SessionInputState::Cancelled
        );
    }

    #[tokio::test]
    async fn every_message_write_path_redacts_json_secrets() {
        let db = Database::connect_in_memory().await.unwrap();
        let session = SessionRecord::new("/r", "g", "m", leveler_core::now());
        SessionRepository::new(&db).create(&session).await.unwrap();
        let id = SessionId::new(session.id);
        let turn = crate::TurnRepository::new(&db)
            .start(&id, "user", None, leveler_core::now())
            .await
            .unwrap();
        let turn_id = leveler_core::TurnId::new(turn.id);
        let secret = r#"{"api_key":"message-secret-value"}"#.to_string();
        let repo = MessageRepository::new(&db);

        repo.append(&id, std::slice::from_ref(&secret), leveler_core::now())
            .await
            .unwrap();
        repo.append_in_turn(
            &id,
            &turn_id,
            std::slice::from_ref(&secret),
            leveler_core::now(),
        )
        .await
        .unwrap();
        repo.replace_all(&id, &[secret], leveler_core::now())
            .await
            .unwrap();

        let stored = repo.load(&id).await.unwrap();
        assert_eq!(stored.len(), 1);
        assert!(!stored[0].contains("message-secret-value"), "{stored:?}");
        assert!(stored[0].contains("[REDACTED]"), "{stored:?}");
    }

    #[tokio::test]
    async fn media_reference_payloads_cover_transcript_turn_and_event_sources() {
        let db = Database::connect_in_memory().await.unwrap();
        let session = SessionRecord::new("/r", "g", "m", leveler_core::now());
        SessionRepository::new(&db).create(&session).await.unwrap();
        let id = SessionId::new(session.id);
        let image_message = r#"{"role":"user","content":[{"type":"image","source":{"kind":"base64","media_type":"image/png","data":"aGk="}}]}"#;
        let plain_message =
            r#"{"role":"assistant","content":[{"type":"text","text":"no image here"}]}"#;
        let turn_payload = r#"{"initiating_message":{"role":"user","content":[{"type":"image","source":{"kind":"base64","media_type":"image/png","data":"aGk="}}]}}"#;
        crate::TurnRepository::new(&db)
            .start(&id, "user", Some(turn_payload), leveler_core::now())
            .await
            .unwrap();
        let repo = MessageRepository::new(&db);
        repo.append(
            &id,
            &[image_message.to_string(), plain_message.to_string()],
            leveler_core::now(),
        )
        .await
        .unwrap();
        let events = crate::EventRepository::new(&db);
        events
            .append(
                &id,
                None,
                "tool_call_images",
                r#"{"payload":{"images":[{"sha256":"abababababababababababababababababababababababababababababababab","mime_type":"image/png"}]},"type":"tool_call_images"}"#,
                leveler_core::now(),
            )
            .await
            .unwrap();
        events
            .append(
                &id,
                None,
                "context_snapshot",
                r#"{"payload":{"messages":[{"role":"user","content":[{"type":"image","source":{"kind":"base64","media_type":"image/png","data":"aGk="}}]}]},"type":"context_snapshot"}"#,
                leveler_core::now(),
            )
            .await
            .unwrap();

        let payloads = repo.media_reference_payloads().await.unwrap();
        assert_eq!(payloads.len(), 4, "{payloads:?}");
        assert_eq!(
            payloads
                .iter()
                .filter(|payload| payload.contains("\"base64\""))
                .count(),
            3
        );
        assert!(
            payloads
                .iter()
                .any(|payload| payload.contains("tool_call_images"))
        );
        assert!(
            !payloads
                .iter()
                .any(|payload| payload.contains("no image here"))
        );
    }
}
