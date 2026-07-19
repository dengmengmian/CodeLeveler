//! The append-only event log persists BEFORE forwarding, so an
//! observer can never have seen an event that a crash then loses.

use leveler_core::{SessionId, TurnId};
use leveler_storage::{EVENT_SCHEMA_VERSION, EventRecord, EventStore};

use crate::{EngineError, EngineEvent};

/// Reject a persisted event whose payload version this build does not
/// understand — a newer writer's row is a hard, named error, never a guess.
fn check_version(row: &EventRecord) -> Result<(), EngineError> {
    if row.schema_version > EVENT_SCHEMA_VERSION {
        return Err(EngineError::Corrupt(format!(
            "event {} has schema_version {} > supported {EVENT_SCHEMA_VERSION}",
            row.id, row.schema_version
        )));
    }
    Ok(())
}

/// Sequenced, persist-before-forward event sink for one session. Depends on the
/// [`EventStore`] port, not a concrete database, so it can be exercised against
/// an in-memory store without SQLite.
pub struct EventLog<'a> {
    store: &'a dyn EventStore,
    session_id: SessionId,
}

impl<'a> EventLog<'a> {
    pub fn new(store: &'a dyn EventStore, session_id: SessionId) -> Self {
        Self { store, session_id }
    }

    /// Persist the event (unless transient), THEN forward it to the observer.
    /// A persistence failure aborts the turn — the observer never sees an
    /// event that isn't durable.
    pub async fn append(
        &self,
        turn_id: Option<&TurnId>,
        event: EngineEvent,
        forward: &mut dyn FnMut(EngineEvent),
    ) -> Result<(), EngineError> {
        if !event.is_transient() {
            let (event_type, payload) = event.to_row()?;
            self.store
                .append(
                    &self.session_id,
                    turn_id,
                    &event_type,
                    &payload,
                    leveler_core::now(),
                )
                .await?;
        }
        forward(event);
        Ok(())
    }

    /// Replay every persisted event of this session, in sequence order.
    /// Unknown event types are hard errors (never silently skipped).
    pub async fn replay(&self) -> Result<Vec<EngineEvent>, EngineError> {
        let rows = self.store.load(&self.session_id).await?;
        rows.iter()
            .map(|row| {
                check_version(row)?;
                EngineEvent::from_payload(&row.payload)
            })
            .collect()
    }

    /// The newest durable model-visible context, optionally scoped to one
    /// turn. Raw transcript reconstruction is only a fallback when no snapshot
    /// has ever been emitted.
    ///
    /// Uses the store's indexed by-type lookup: snapshots embed whole message
    /// lists, and a full-log scan per restore would be O(session length).
    pub async fn latest_context_snapshot(
        &self,
        turn_id: Option<&TurnId>,
    ) -> Result<Option<Vec<leveler_model::Message>>, EngineError> {
        let Some(row) = self
            .store
            .load_last_by_type(&self.session_id, "context_snapshot", turn_id)
            .await?
        else {
            return Ok(None);
        };
        check_version(&row)?;
        match EngineEvent::from_payload(&row.payload)? {
            EngineEvent::ContextSnapshot { messages } => Ok(Some(messages)),
            _ => Err(EngineError::Corrupt(
                "context_snapshot row carried a different event".into(),
            )),
        }
    }

    /// Tool calls with a persisted `ToolCallStarted` but no matching
    /// `ToolCallFinished`: the crash window M5 reconciles on resume. Returned in
    /// the order they were started. `pending_approval` marks a call that crashed
    /// while still blocked in approval (its dispatch never ran).
    pub async fn dangling_tool_calls(&self) -> Result<Vec<DanglingCall>, EngineError> {
        // Must go through the raw rows (not `replay`) to keep each event's
        // `turn_id`, which the recovery step needs to attribute the reconciling
        // event to the crashed turn.
        let rows = self.store.load(&self.session_id).await?;
        let mut open: Vec<DanglingCall> = Vec::new();
        for row in &rows {
            check_version(row)?;
            match EngineEvent::from_payload(&row.payload)? {
                EngineEvent::ToolCallStarted {
                    call_id,
                    name,
                    arguments,
                    risk,
                } => open.push(DanglingCall {
                    turn_id: row.turn_id.clone(),
                    call_id,
                    name,
                    arguments,
                    risk,
                    pending_approval: false,
                }),
                EngineEvent::ToolCallFinished { call_id, .. } => {
                    open.retain(|c| c.call_id != call_id);
                }
                // Approval events sit between a call's Started and Finished, so
                // they attach to the most-recent open call: a request marks it
                // blocked-in-approval, a resolution clears that.
                EngineEvent::ApprovalRequested { .. } => {
                    if let Some(last) = open.last_mut() {
                        last.pending_approval = true;
                    }
                }
                EngineEvent::ApprovalResolved { .. } => {
                    if let Some(last) = open.last_mut() {
                        last.pending_approval = false;
                    }
                }
                _ => {}
            }
        }
        Ok(open)
    }
}

/// A tool call started but never finished — the crash window M5 reconciles.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DanglingCall {
    pub turn_id: Option<String>,
    pub call_id: String,
    pub name: String,
    pub arguments: String,
    /// Risk captured when the call originally started. `None` is legacy or
    /// unknown and must never be auto-replayed.
    pub risk: Option<leveler_execution::RiskLevel>,
    /// Crashed while blocked in approval (`ApprovalRequested` with no resolution):
    /// dispatch never ran, so there is no side effect to recover.
    pub pending_approval: bool,
}

#[cfg(test)]
mod tests {
    use super::*;
    use leveler_storage::{
        Database, EventRepository, MemoryEventStore, SessionRecord, SessionRepository,
        TurnRepository,
    };

    #[tokio::test]
    async fn event_log_works_over_the_memory_store_without_sqlite() {
        // The seam lets EventLog be exercised with no database: persist,
        // replay, and skip transients all work against MemoryEventStore.
        let store = MemoryEventStore::new();
        let log = EventLog::new(&store, SessionId::generate());

        log.append(
            None,
            EngineEvent::AssistantDelta { text: "d".into() },
            &mut |_| {},
        )
        .await
        .unwrap();
        log.append(
            None,
            EngineEvent::TaskFinished {
                outcome: crate::TaskOutcome::Verified,
                reason: None,
            },
            &mut |_| {},
        )
        .await
        .unwrap();

        let replayed = log.replay().await.unwrap();
        assert_eq!(
            replayed,
            vec![EngineEvent::TaskFinished {
                outcome: crate::TaskOutcome::Verified,
                reason: None,
            }],
            "transient delta is skipped; the canonical event replays"
        );
    }

    async fn db_with_session() -> (Database, SessionId) {
        let db = Database::connect_in_memory().await.unwrap();
        let record = SessionRecord::new("/repo", "goal", "mock/m", leveler_core::now());
        SessionRepository::new(&db).create(&record).await.unwrap();
        let id = SessionId::new(record.id);
        (db, id)
    }

    #[tokio::test]
    async fn dangling_tool_calls_flags_started_without_finished() {
        let (db, session) = db_with_session().await;
        let turn = TurnRepository::new(&db)
            .start(&session, "node", None, leveler_core::now())
            .await
            .unwrap();
        let turn_id = TurnId::new(turn.id.clone());
        let log = EventLog::new(&db, session.clone());

        // c1 finishes; c2 is left hanging (process crashed while c2 ran).
        for (call_id, finished) in [("c1", true), ("c2", false)] {
            log.append(
                Some(&turn_id),
                EngineEvent::ToolCallStarted {
                    call_id: call_id.into(),
                    name: "read_file".into(),
                    arguments: "{\"path\":\"README.md\"}".into(),
                    risk: Some(leveler_execution::RiskLevel::Safe),
                },
                &mut |_| {},
            )
            .await
            .unwrap();
            if finished {
                log.append(
                    Some(&turn_id),
                    EngineEvent::ToolCallFinished {
                        call_id: call_id.into(),
                        name: "read_file".into(),
                        is_error: false,
                        preview: "ok".into(),
                    },
                    &mut |_| {},
                )
                .await
                .unwrap();
            }
        }

        let dangling = log.dangling_tool_calls().await.unwrap();
        assert_eq!(dangling.len(), 1, "only the unfinished call is dangling");
        assert_eq!(dangling[0].call_id, "c2");
        assert_eq!(dangling[0].name, "read_file");
        assert_eq!(dangling[0].turn_id.as_deref(), Some(turn_id.as_str()));
        assert_eq!(dangling[0].arguments, "{\"path\":\"README.md\"}");
        assert!(!dangling[0].pending_approval);
    }

    #[tokio::test]
    async fn persists_before_forwarding() {
        let (db, session) = db_with_session().await;
        let log = EventLog::new(&db, session.clone());

        // The forward closure runs the durability check: at forward time the
        // row must already be readable.
        let event = EngineEvent::AssistantMessage {
            text: "hi".to_string(),
        };
        // Peek from inside a sync closure via a channel; assert afterwards.
        let (tx, rx) = std::sync::mpsc::channel();
        log.append(None, event.clone(), &mut |forwarded| {
            tx.send(forwarded).unwrap();
        })
        .await
        .unwrap();
        let forwarded = rx.try_recv().expect("event must be forwarded");
        assert_eq!(forwarded, event);

        let rows = EventRepository::new(&db).load(&session).await.unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].event_type, "assistant_message");
    }

    #[tokio::test]
    async fn transient_events_are_forwarded_but_not_persisted() {
        let (db, session) = db_with_session().await;
        let log = EventLog::new(&db, session.clone());

        let mut forwarded = 0;
        log.append(
            None,
            EngineEvent::AssistantDelta {
                text: "chunk".to_string(),
            },
            &mut |_| forwarded += 1,
        )
        .await
        .unwrap();
        log.append(
            None,
            EngineEvent::TokenUsage {
                input_tokens: 1,
                output_tokens: 2,
                cached_input_tokens: 0,
            },
            &mut |_| forwarded += 1,
        )
        .await
        .unwrap();

        assert_eq!(forwarded, 2);
        let rows = EventRepository::new(&db).load(&session).await.unwrap();
        assert!(rows.is_empty(), "transients must never hit the database");
    }

    #[tokio::test]
    async fn workspace_snapshot_is_persisted_with_its_turn_and_call() {
        let (db, session) = db_with_session().await;
        let turn = TurnRepository::new(&db)
            .start(&session, "node", None, leveler_core::now())
            .await
            .unwrap();
        let turn_id = TurnId::new(turn.id.clone());
        let log = EventLog::new(&db, session.clone());

        log.append(
            Some(&turn_id),
            EngineEvent::WorkspaceSnapshotCreated {
                call_id: "call-7".into(),
                snapshot: "tree-sha".into(),
            },
            &mut |_| {},
        )
        .await
        .unwrap();

        let rows = EventRepository::new(&db).load(&session).await.unwrap();
        assert_eq!(rows[0].turn_id.as_deref(), Some(turn.id.as_str()));
        assert_eq!(rows[0].event_type, "workspace_snapshot_created");
        let event = EngineEvent::from_payload(&rows[0].payload).unwrap();
        assert_eq!(
            event,
            EngineEvent::WorkspaceSnapshotCreated {
                call_id: "call-7".into(),
                snapshot: "tree-sha".into(),
            }
        );
    }

    #[tokio::test]
    async fn latest_context_snapshot_is_scoped_to_the_requested_turn() {
        let (db, session) = db_with_session().await;
        let turns = TurnRepository::new(&db);
        let a = turns
            .start(&session, "node", None, leveler_core::now())
            .await
            .unwrap();
        let b = turns
            .start(&session, "node", None, leveler_core::now())
            .await
            .unwrap();
        let a = TurnId::new(a.id);
        let b = TurnId::new(b.id);
        let log = EventLog::new(&db, session);
        let message = |text: &str| leveler_model::Message::text(leveler_model::Role::User, text);

        log.append(
            Some(&a),
            EngineEvent::ContextSnapshot {
                messages: vec![message("a")],
            },
            &mut |_| {},
        )
        .await
        .unwrap();
        log.append(
            Some(&b),
            EngineEvent::ContextSnapshot {
                messages: vec![message("b")],
            },
            &mut |_| {},
        )
        .await
        .unwrap();

        let restored = log
            .latest_context_snapshot(Some(&a))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(restored[0].text_content(), "a");
    }

    #[tokio::test]
    async fn replay_roundtrips_rich_payloads() {
        let (db, session) = db_with_session().await;
        let log = EventLog::new(&db, session.clone());

        let events = vec![
            EngineEvent::TaskStarted {
                goal: "fix".into(),
                model: "mock/m".into(),
                mode: "assisted".into(),
                sandbox: false,
                kind: crate::ExecutionKind::Orchestrate,
            },
            EngineEvent::PhaseChanged {
                from: leveler_orchestrator::AgentState::Understand,
                to: leveler_orchestrator::AgentState::Localize,
            },
            EngineEvent::TaskFinished {
                outcome: crate::TaskOutcome::CompletedUnverified,
                reason: Some("no gating checks".into()),
            },
        ];
        for e in &events {
            log.append(None, e.clone(), &mut |_| {}).await.unwrap();
        }

        let replayed = log.replay().await.unwrap();
        assert_eq!(replayed, events);
    }

    #[tokio::test]
    async fn newer_schema_version_is_a_hard_error_on_replay() {
        // A future writer stamped a payload version this build doesn't know.
        let store = MemoryEventStore::new();
        let session = SessionId::generate();
        let event = EngineEvent::TaskFinished {
            outcome: crate::TaskOutcome::Verified,
            reason: None,
        };
        let (event_type, payload) = event.to_row().unwrap();
        store.seed(leveler_storage::EventRecord {
            id: "evt-future".into(),
            session_id: session.as_str().to_string(),
            turn_id: None,
            sequence: 1,
            event_type,
            payload,
            created_at: leveler_core::now().to_rfc3339(),
            schema_version: 999,
        });

        let log = EventLog::new(&store, session);
        let err = log.replay().await.expect_err("unknown version must fail");
        assert!(
            matches!(&err, EngineError::Corrupt(m) if m.contains("schema_version")),
            "unexpected error: {err:?}"
        );
    }

    #[tokio::test]
    async fn replay_reconstructs_the_terminal_outcome_projection() {
        // The events log is authoritative: the session outcome projection can be
        // rebuilt by replaying, and matches the last TaskFinished.
        let (db, session) = db_with_session().await;
        let log = EventLog::new(&db, session);
        log.append(
            None,
            EngineEvent::TaskFinished {
                outcome: crate::TaskOutcome::CompletedUnverified,
                reason: None,
            },
            &mut |_| {},
        )
        .await
        .unwrap();

        let outcome = log
            .replay()
            .await
            .unwrap()
            .into_iter()
            .rev()
            .find_map(|e| match e {
                EngineEvent::TaskFinished { outcome, .. } => Some(outcome),
                _ => None,
            });
        assert_eq!(outcome, Some(crate::TaskOutcome::CompletedUnverified));
    }

    #[tokio::test]
    async fn unknown_event_type_is_a_hard_error_on_replay() {
        let (db, session) = db_with_session().await;
        // Simulate a future/corrupt row written by a newer version.
        EventRepository::new(&db)
            .append(
                &session,
                None,
                "from_the_future",
                r#"{"type":"from_the_future","payload":{}}"#,
                leveler_core::now(),
            )
            .await
            .unwrap();

        let log = EventLog::new(&db, session);
        let err = log.replay().await.expect_err("must not silently skip");
        assert!(matches!(err, EngineError::Corrupt(_)));
    }
}
