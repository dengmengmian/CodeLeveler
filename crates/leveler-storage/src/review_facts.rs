//! Read-only session facts for manual review.
//!
//! These queries expose the durable rows a review needs to judge an execution:
//! goals, their accepted requirement changes, and checkpoints. They are reads
//! of the same rows the runtime owns — not a second store and not a second
//! source of truth. Nothing here writes, resumes or re-drives a session.

use leveler_core::SessionId;

use crate::database::{Database, StorageError};

/// One goal of a reviewed session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReviewGoalFact {
    /// Stable goal identity.
    pub id: String,
    /// What the user asked, verbatim.
    pub objective: String,
    /// `running` or `settled`.
    pub state: String,
    /// When the goal was opened, RFC 3339.
    pub opened_at: String,
    /// When the goal stopped owing work, if it has.
    pub settled_at: Option<String>,
    /// Work windows consumed.
    pub windows_run: i64,
    /// The model's declared acceptance checks.
    pub acceptance: Vec<String>,
    /// Verbatim user amendments to the scope.
    pub amendments: Vec<String>,
    /// When the user's stage pause was reached, if it was.
    pub user_paused_at: Option<String>,
    /// The structured pause boundary, as stored JSON.
    pub pause_boundary: Option<String>,
}

/// One accepted requirement change on a goal.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReviewRequirementFact {
    /// Per-goal sequence.
    pub seq: i64,
    /// The turn whose newest user input the quote came from.
    pub turn_id: String,
    /// The accepted operation (`add`, `waive`, `replace`, …).
    pub operation: String,
    /// The requirement it applies to.
    pub requirement_id: String,
    /// Requirement kind, when one was declared.
    pub kind: Option<String>,
    /// Requirement text, when one was declared.
    pub text: Option<String>,
    /// Replacement target, when the operation names one.
    pub target: Option<String>,
    /// The span the model quoted from the user's own message.
    pub source_quote: String,
    /// The model's stated rationale, when it gave one.
    pub reason: Option<String>,
    /// When the declaration was accepted, RFC 3339.
    pub created_at: String,
}

/// Row counts for one reviewed session. Counts only: they say how much
/// evidence exists, never whether it is good.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ReviewSessionCounts {
    /// Admitted turns.
    pub turns: i64,
    /// Persisted events.
    pub events: i64,
    /// Persisted session messages.
    pub messages: i64,
    /// Recorded model requests.
    pub model_requests: i64,
}

/// One durable goal checkpoint, with the payload the runtime wrote.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReviewCheckpointFact {
    /// Stable checkpoint identity.
    pub id: String,
    /// The goal this checkpoint belongs to.
    pub goal_id: String,
    /// `manual`, `milestone`, `context_compaction` or `interrupted`.
    pub reason: String,
    /// How much of the event log the checkpoint represents.
    pub event_cursor: i64,
    /// The checkpoint payload exactly as the runtime wrote it.
    pub payload: String,
    /// When it was recorded, RFC 3339.
    pub created_at: String,
}

impl Database {
    /// The session row's recorded outcome, if one was written.
    pub async fn review_session_outcome(
        &self,
        session_id: &SessionId,
    ) -> Result<Option<String>, StorageError> {
        Ok(
            sqlx::query_scalar("SELECT outcome FROM sessions WHERE id = ?1")
                .bind(session_id.as_str())
                .fetch_optional(self.pool())
                .await?
                .flatten(),
        )
    }

    /// Row counts for one reviewed session.
    pub async fn review_counts(
        &self,
        session_id: &SessionId,
    ) -> Result<ReviewSessionCounts, StorageError> {
        let turns: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM turns WHERE session_id = ?1")
            .bind(session_id.as_str())
            .fetch_one(self.pool())
            .await?;
        let events: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM events WHERE session_id = ?1")
            .bind(session_id.as_str())
            .fetch_one(self.pool())
            .await?;
        let messages: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM session_messages WHERE session_id = ?1")
                .bind(session_id.as_str())
                .fetch_one(self.pool())
                .await?;
        let model_requests: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM model_requests WHERE session_id = ?1")
                .bind(session_id.as_str())
                .fetch_one(self.pool())
                .await?;
        Ok(ReviewSessionCounts {
            turns,
            events,
            messages,
            model_requests,
        })
    }

    /// Goals opened on a reviewed session, oldest first.
    pub async fn review_goals(
        &self,
        session_id: &SessionId,
    ) -> Result<Vec<ReviewGoalFact>, StorageError> {
        let rows: Vec<(
            String,
            String,
            String,
            String,
            Option<String>,
            i64,
            String,
            String,
            Option<String>,
            Option<String>,
        )> = sqlx::query_as(
            "SELECT goals.id, goals.objective, goals.state, goals.opened_at, goals.settled_at, \
                    goals.windows_run, goals.acceptance, goals.amendments, goals.user_paused_at, \
                    goals.pause_boundary \
             FROM goals JOIN tasks ON tasks.id = goals.task_id \
             WHERE tasks.session_id = ?1 ORDER BY goals.opened_at, goals.rowid",
        )
        .bind(session_id.as_str())
        .fetch_all(self.pool())
        .await?;
        rows.into_iter()
            .map(
                |(
                    id,
                    objective,
                    state,
                    opened_at,
                    settled_at,
                    windows_run,
                    acceptance,
                    amendments,
                    user_paused_at,
                    pause_boundary,
                )| {
                    Ok(ReviewGoalFact {
                        id,
                        objective,
                        state,
                        opened_at,
                        settled_at,
                        windows_run,
                        acceptance: string_list(&acceptance)?,
                        amendments: string_list(&amendments)?,
                        user_paused_at,
                        pause_boundary,
                    })
                },
            )
            .collect()
    }

    /// Requirement changes accepted on one goal, in order.
    pub async fn review_goal_requirements(
        &self,
        goal_id: &str,
    ) -> Result<Vec<ReviewRequirementFact>, StorageError> {
        let rows: Vec<(
            i64,
            String,
            String,
            String,
            Option<String>,
            Option<String>,
            Option<String>,
            String,
            Option<String>,
            String,
        )> = sqlx::query_as(
            "SELECT seq, turn_id, operation, requirement_id, kind, text, target, source_quote, \
                    reason, created_at \
             FROM goal_requirement_events WHERE goal_id = ?1 ORDER BY seq",
        )
        .bind(goal_id)
        .fetch_all(self.pool())
        .await?;
        Ok(rows
            .into_iter()
            .map(
                |(
                    seq,
                    turn_id,
                    operation,
                    requirement_id,
                    kind,
                    text,
                    target,
                    source_quote,
                    reason,
                    created_at,
                )| ReviewRequirementFact {
                    seq,
                    turn_id,
                    operation,
                    requirement_id,
                    kind,
                    text,
                    target,
                    source_quote,
                    reason,
                    created_at,
                },
            )
            .collect())
    }

    /// Checkpoints recorded for a reviewed session, newest boundary first.
    pub async fn review_checkpoints(
        &self,
        session_id: &SessionId,
    ) -> Result<Vec<ReviewCheckpointFact>, StorageError> {
        let rows: Vec<(String, String, String, i64, String, String)> = sqlx::query_as(
            "SELECT id, goal_id, reason, event_cursor, payload, created_at \
             FROM goal_checkpoints WHERE session_id = ?1 \
             ORDER BY event_cursor DESC, created_at DESC, rowid DESC",
        )
        .bind(session_id.as_str())
        .fetch_all(self.pool())
        .await?;
        Ok(rows
            .into_iter()
            .map(
                |(id, goal_id, reason, event_cursor, payload, created_at)| ReviewCheckpointFact {
                    id,
                    goal_id,
                    reason,
                    event_cursor,
                    payload,
                    created_at,
                },
            )
            .collect())
    }
}

fn string_list(payload: &str) -> Result<Vec<String>, StorageError> {
    serde_json::from_str::<Vec<String>>(payload).map_err(|error| {
        StorageError::InvalidData(format!("stored string list is not valid JSON: {error}"))
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{SessionRecord, SessionRepository};

    async fn session(db: &Database) -> SessionId {
        let record = SessionRecord::new("/r", "goal", "m", leveler_core::now());
        SessionRepository::new(db).create(&record).await.unwrap();
        SessionId::new(record.id)
    }

    #[tokio::test]
    async fn goals_are_read_from_their_own_task_and_checkpoints_from_the_session() {
        let db = Database::connect_in_memory().await.unwrap();
        let session = session(&db).await;
        // Create the task row the goal's foreign key needs.
        sqlx::query("INSERT INTO tasks (id, session_id, created_at) VALUES ('t1', ?1, ?2)")
            .bind(session.as_str())
            .bind(leveler_core::now().to_rfc3339())
            .execute(db.pool())
            .await
            .unwrap();
        sqlx::query(
            "INSERT INTO goals (id, task_id, objective, state, opened_at, windows_run, acceptance, amendments) \
             VALUES ('g1','t1','ship it','running','2026-01-01T00:00:00Z',2,'[\"cargo test\"]','[\"also docs\"]')",
        )
        .execute(db.pool())
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO goal_requirement_events \
             (goal_id, seq, turn_id, operation, requirement_id, kind, text, source_quote, created_at) \
             VALUES ('g1',1,'turn-1','add','r1','functional','do x','user said x','2026-01-01T00:00:01Z')",
        )
        .execute(db.pool())
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO goal_checkpoints (id, goal_id, session_id, reason, event_cursor, schema_version, payload, created_at) \
             VALUES ('c1','g1',?1,'manual',7,1,'{\"summary\":\"halfway\"}','2026-01-01T00:00:02Z')",
        )
        .bind(session.as_str())
        .execute(db.pool())
        .await
        .unwrap();

        let goals = db.review_goals(&session).await.unwrap();
        assert_eq!(goals.len(), 1);
        assert_eq!(goals[0].objective, "ship it");
        assert_eq!(goals[0].windows_run, 2);
        assert_eq!(goals[0].acceptance, vec!["cargo test"]);
        let requirements = db.review_goal_requirements("g1").await.unwrap();
        assert_eq!(requirements.len(), 1);
        assert_eq!(requirements[0].operation, "add");
        let checkpoints = db.review_checkpoints(&session).await.unwrap();
        assert_eq!(checkpoints.len(), 1);
        assert!(checkpoints[0].payload.contains("halfway"));
    }

    #[tokio::test]
    async fn another_sessions_goals_are_not_returned() {
        let db = Database::connect_in_memory().await.unwrap();
        let mine = session(&db).await;
        let other = session(&db).await;
        for (task, session) in [("t1", &mine), ("t2", &other)] {
            sqlx::query("INSERT INTO tasks (id, session_id, created_at) VALUES (?1, ?2, ?3)")
                .bind(task)
                .bind(session.as_str())
                .bind(leveler_core::now().to_rfc3339())
                .execute(db.pool())
                .await
                .unwrap();
        }
        sqlx::query(
            "INSERT INTO goals (id, task_id, objective, state, opened_at) VALUES \
             ('g1','t1','mine','running','2026-01-01T00:00:00Z'), \
             ('g2','t2','other','running','2026-01-01T00:00:00Z')",
        )
        .execute(db.pool())
        .await
        .unwrap();
        let goals = db.review_goals(&mine).await.unwrap();
        assert_eq!(
            goals
                .iter()
                .map(|goal| goal.id.as_str())
                .collect::<Vec<_>>(),
            vec!["g1"]
        );
    }
}
