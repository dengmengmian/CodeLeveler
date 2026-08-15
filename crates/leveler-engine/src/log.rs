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

/// Decode one persisted row, fail-close with FULL provenance on corruption.
///
/// The event log is the authoritative plane: a row that cannot be decoded is
/// never silently skipped (R007 F2 policy). The error names the session,
/// sequence, and event type so a corrupt legacy row is diagnosable and
/// repairable offline — but it must never echo payload bytes, which may
/// contain exactly the secret material redaction was trying to remove.
fn decode_row(row: &EventRecord) -> Result<EngineEvent, EngineError> {
    check_version(row)?;
    EngineEvent::from_payload(&row.payload).map_err(|e| {
        EngineError::Corrupt(format!(
            "corrupt authoritative event: session {} sequence {} type '{}': {e}",
            row.session_id, row.sequence, row.event_type
        ))
    })
}

/// Sequenced, persist-before-forward event sink for one session. Depends on the
/// [`EventStore`] port, not a concrete database, so it can be exercised against
/// an in-memory store without SQLite.
pub struct EventLog<'a> {
    store: &'a dyn EventStore,
    session_id: SessionId,
    /// When set, every persisted append is fenced on this token being the
    /// task's current ownership - a stale runtime cannot extend the log.
    owner: Option<leveler_core::OwnershipToken>,
}

impl<'a> EventLog<'a> {
    pub fn new(store: &'a dyn EventStore, session_id: SessionId) -> Self {
        Self {
            store,
            session_id,
            owner: None,
        }
    }

    /// An ownership-fenced log: appends carry `token` and are rejected
    /// atomically once the token is stale.
    pub fn new_owned(
        store: &'a dyn EventStore,
        session_id: SessionId,
        token: leveler_core::OwnershipToken,
    ) -> Self {
        Self {
            store,
            session_id,
            owner: Some(token),
        }
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
            match &self.owner {
                Some(token) => {
                    self.store
                        .append_owned(
                            token,
                            &self.session_id,
                            turn_id,
                            &event_type,
                            &payload,
                            leveler_core::now(),
                        )
                        .await?;
                }
                None => {
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
            }
        }
        forward(event);
        Ok(())
    }

    /// Replay every persisted event of this session, in sequence order.
    /// Unknown event types are hard errors (never silently skipped).
    pub async fn replay(&self) -> Result<Vec<EngineEvent>, EngineError> {
        let rows = self.store.load(&self.session_id).await?;
        rows.iter().map(decode_row).collect()
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
    ) -> Result<Option<SnapshotView>, EngineError> {
        let Some(row) = self
            .store
            .load_last_by_type(&self.session_id, "context_snapshot", turn_id)
            .await?
        else {
            return Ok(None);
        };
        match decode_row(&row)? {
            EngineEvent::ContextSnapshot {
                messages,
                through_ordinal,
            } => Ok(Some(SnapshotView {
                messages,
                through_ordinal,
            })),
            _ => Err(EngineError::Corrupt(
                "context_snapshot row carried a different event".into(),
            )),
        }
    }

    /// The highest fold threshold this session has durably expanded to
    /// (C5-S3). Budgets only climb within a task, so the LAST
    /// `ContextExpanded` event carries the maximum; `None` means the session
    /// never expanded and the initial tier stands.
    pub async fn max_expanded_context_budget(&self) -> Result<Option<u32>, EngineError> {
        let Some(row) = self
            .store
            .load_last_by_type(&self.session_id, "context_expanded", None)
            .await?
        else {
            return Ok(None);
        };
        match decode_row(&row)? {
            EngineEvent::ContextExpanded { to, .. } => Ok(Some(to)),
            _ => Err(EngineError::Corrupt(
                "context_expanded row carried a different event".into(),
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
            match decode_row(row)? {
                EngineEvent::ToolCallStarted {
                    call_id,
                    name,
                    arguments,
                    parallel: _,
                    risk,
                    agent_id,
                } => open.push(DanglingCall {
                    turn_id: row.turn_id.clone(),
                    call_id,
                    name,
                    arguments,
                    risk,
                    agent_id,
                    pending_approval: false,
                }),
                EngineEvent::ToolCallFinished {
                    call_id, agent_id, ..
                } => {
                    // Pair on (agent, call), not call alone. Call ids are
                    // local to the agent that made them, so two concurrent
                    // sub-agents can easily produce the same one — and a
                    // call-id-only match would let one child's finish close
                    // the other child's dangling record, hiding a side effect
                    // recovery must reconcile.
                    open.retain(|c| !(c.call_id == call_id && c.agent_id == agent_id));
                }
                // An approval attaches to the call it was raised for. Pairing
                // on (agent, call) matters for the same reason as above: with
                // concurrent delegated agents, "the most recent open call" is
                // routinely somebody else's.
                EngineEvent::ApprovalRequested {
                    call_id, agent_id, ..
                } => set_pending(&mut open, &call_id, &agent_id, true),
                EngineEvent::ApprovalResolved {
                    call_id, agent_id, ..
                } => set_pending(&mut open, &call_id, &agent_id, false),
                _ => {}
            }
        }
        Ok(open)
    }
}

/// Mark (or unmark) the open call an approval was raised for.
///
/// Rows written before approvals carried attribution have no ids at all; those
/// fall back to the most recent open call, which is what the single-agent
/// sessions they come from actually mean. Dropping the marker instead would
/// silently turn "crashed while blocked in approval, so the side effect never
/// ran" into "may have run" — the more dangerous of the two readings.
fn set_pending(
    open: &mut [DanglingCall],
    call_id: &Option<String>,
    agent_id: &Option<String>,
    pending: bool,
) {
    let target = match call_id {
        Some(call_id) => open
            .iter_mut()
            .rev()
            .find(|c| c.call_id == *call_id && c.agent_id == *agent_id),
        None => open.last_mut(),
    };
    if let Some(call) = target {
        call.pending_approval = pending;
    }
}

/// A persisted context snapshot plus the transcript watermark it supersedes.
/// `through_ordinal: Some(n)` means restore appends exactly the transcript
/// messages after the first `n`; `None` (legacy / executor in-loop snapshots)
/// falls back to the suffix-overlap merge heuristic.
#[derive(Debug, Clone, PartialEq)]
pub struct SnapshotView {
    pub messages: Vec<leveler_model::Message>,
    pub through_ordinal: Option<u64>,
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
    /// The delegated agent that made this call, when it was not the top-level
    /// one. Recovery reports it so a human裁决 knows whose side effect it is.
    pub agent_id: Option<String>,
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
                stop: None,
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
                stop: None,
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
                    parallel: false,
                    risk: Some(leveler_execution::RiskLevel::Safe),
                    agent_id: None,
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
                        agent_id: None,
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

    /// Call ids are local to the agent that produced them, so two concurrent
    /// delegated agents routinely produce the same one. An approval must mark
    /// the call it was actually raised for — attributing it to whichever call
    /// opened most recently tells recovery the wrong story: one child looks
    /// blocked in approval (its side effect never ran) while the child that
    /// really was blocked looks like it may have run.
    #[tokio::test]
    async fn an_approval_marks_its_own_call_not_the_most_recent_one() {
        let (db, session) = db_with_session().await;
        let turn = TurnRepository::new(&db)
            .start(&session, "node", None, leveler_core::now())
            .await
            .unwrap();
        let turn_id = TurnId::new(turn.id.clone());
        let log = EventLog::new(&db, session.clone());

        // Both children pick the id "call-1"; alpha's is the one that blocks.
        for agent in ["alpha", "beta"] {
            log.append(
                Some(&turn_id),
                EngineEvent::ToolCallStarted {
                    call_id: "call-1".into(),
                    name: "run_command".into(),
                    arguments: "{}".into(),
                    parallel: false,
                    risk: Some(leveler_execution::RiskLevel::Destructive),
                    agent_id: Some(agent.to_string()),
                },
                &mut |_| {},
            )
            .await
            .unwrap();
        }

        log.append(
            Some(&turn_id),
            EngineEvent::ApprovalRequested {
                id: leveler_core::ApprovalId::generate(),
                call_id: Some("call-1".into()),
                agent_id: Some("alpha".into()),
                tool: "run_command".into(),
                summary: String::new(),
                command: Some("rm -rf build".into()),
                risk: "Destructive".into(),
            },
            &mut |_| {},
        )
        .await
        .unwrap();

        let dangling = log.dangling_tool_calls().await.unwrap();
        assert_eq!(dangling.len(), 2, "both children are still open");
        let alpha = dangling
            .iter()
            .find(|c| c.agent_id.as_deref() == Some("alpha"))
            .expect("alpha's call");
        let beta = dangling
            .iter()
            .find(|c| c.agent_id.as_deref() == Some("beta"))
            .expect("beta's call");
        assert!(
            alpha.pending_approval,
            "the approval was raised for alpha's call"
        );
        assert!(
            !beta.pending_approval,
            "beta merely started later; it was never blocked in approval"
        );
    }

    /// Legacy rows carry no call attribution. Falling back to the most recent
    /// open call keeps old sessions recoverable instead of silently losing the
    /// blocked-in-approval marker.
    #[tokio::test]
    async fn an_unattributed_approval_still_marks_the_most_recent_call() {
        let (db, session) = db_with_session().await;
        let turn = TurnRepository::new(&db)
            .start(&session, "node", None, leveler_core::now())
            .await
            .unwrap();
        let turn_id = TurnId::new(turn.id.clone());
        let log = EventLog::new(&db, session.clone());

        log.append(
            Some(&turn_id),
            EngineEvent::ToolCallStarted {
                call_id: "c1".into(),
                name: "run_command".into(),
                arguments: "{}".into(),
                parallel: false,
                risk: Some(leveler_execution::RiskLevel::Destructive),
                agent_id: None,
            },
            &mut |_| {},
        )
        .await
        .unwrap();
        log.append(
            Some(&turn_id),
            EngineEvent::ApprovalRequested {
                id: leveler_core::ApprovalId::generate(),
                call_id: None,
                agent_id: None,
                tool: "run_command".into(),
                summary: String::new(),
                command: None,
                risk: "Destructive".into(),
            },
            &mut |_| {},
        )
        .await
        .unwrap();

        let dangling = log.dangling_tool_calls().await.unwrap();
        assert_eq!(dangling.len(), 1);
        assert!(dangling[0].pending_approval);
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
                through_ordinal: Some(1),
            },
            &mut |_| {},
        )
        .await
        .unwrap();
        log.append(
            Some(&b),
            EngineEvent::ContextSnapshot {
                messages: vec![message("b")],
                through_ordinal: Some(2),
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
        assert_eq!(restored.messages[0].text_content(), "a");
        assert_eq!(restored.through_ordinal, Some(1));
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
                kind: crate::ExecutionKind::Direct,
                task_id: Some(leveler_core::TaskId::new("task-1")),
            },
            EngineEvent::PhaseChanged {
                from: leveler_lifecycle::AgentState::Understand,
                to: leveler_lifecycle::AgentState::Localize,
            },
            EngineEvent::TaskFinished {
                outcome: crate::TaskOutcome::CompletedUnverified,
                reason: Some("no gating checks".into()),
                stop: None,
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
            stop: None,
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
                stop: None,
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

    /// R007 F2 whole-session regression: normal events + a secret-bearing
    /// narration + more normal events must persist, stay gapless, and replay
    /// end-to-end with the secret gone (§40 — not just a single-event parse).
    #[tokio::test]
    async fn whole_session_replays_across_a_secret_bearing_event() {
        let (db, session) = db_with_session().await;
        let log = EventLog::new(&db, session.clone());
        let events = [
            EngineEvent::AssistantMessage {
                text: "starting".into(),
            },
            // The R007 accident narration (ends with `PASSWORD:` at the string
            // boundary) plus a real secret in the same session.
            EngineEvent::AssistantMessage {
                text: "Secrets 标签页有 \"Add new\" 按钮。点击添加 secret PASSWORD:".into(),
            },
            EngineEvent::AssistantMessage {
                text: "and the value is password: hunter2-durable-secret".into(),
            },
            EngineEvent::AssistantMessage { text: "done".into() },
        ];
        for event in events {
            log.append(None, event, &mut |_| {}).await.unwrap();
        }

        let rows = EventRepository::new(&db).load(&session).await.unwrap();
        assert_eq!(
            rows.iter().map(|r| r.sequence).collect::<Vec<_>>(),
            vec![1, 2, 3, 4],
            "gapless"
        );
        assert!(
            rows.iter().all(|r| !r.payload.contains("hunter2-durable-secret")),
            "secret must not persist"
        );

        let replayed = log.replay().await.expect("whole session must replay");
        assert_eq!(replayed.len(), 4);
        assert!(matches!(
            &replayed[1],
            EngineEvent::AssistantMessage { text } if text.ends_with("PASSWORD:")
        ));
    }

    /// A legacy row corrupted by the pre-fix scrubber (injected via raw SQL,
    /// below the validating write boundary) must fail replay CLOSED with full
    /// provenance — session, sequence, event type — and must not echo payload
    /// bytes (which may contain the secret redaction failed to remove).
    #[tokio::test]
    async fn corrupt_legacy_row_fails_closed_with_provenance_and_no_payload_echo() {
        let store = MemoryEventStore::new();
        let session = SessionId::generate();
        let log = EventLog::new(&store, session.clone());
        log.append(
            None,
            EngineEvent::AssistantMessage { text: "ok".into() },
            &mut |_| {},
        )
        .await
        .unwrap();
        // The exact malformed bytes R007 persisted (truncated structure),
        // injected BELOW the validating write boundary as a legacy row.
        let corrupt = r#"{"payload":{"text":"secret PASSWORD:"[REDACTED]"type":"assistant_message"}"#;
        store.inject_legacy_row_for_tests(leveler_storage::EventRecord {
            id: leveler_core::EventId::generate().into_inner(),
            session_id: session.as_str().to_string(),
            turn_id: None,
            sequence: 2,
            event_type: "assistant_message".to_string(),
            payload: corrupt.to_string(),
            created_at: leveler_core::now().to_rfc3339(),
            schema_version: 1,
        });

        let err = log.replay().await.expect_err("corrupt row must fail closed");
        let msg = err.to_string();
        assert!(msg.contains("corrupt authoritative event"), "{msg}");
        assert!(msg.contains(session.as_str()), "session provenance: {msg}");
        assert!(msg.contains("sequence 2"), "sequence provenance: {msg}");
        assert!(msg.contains("assistant_message"), "type provenance: {msg}");
        assert!(
            !msg.contains("[REDACTED]") && !msg.contains("PASSWORD"),
            "diagnostics must not echo payload bytes: {msg}"
        );
        // The same fail-close applies to the dangling-call scan on resume.
        let err = log.dangling_tool_calls().await.expect_err("fail closed");
        assert!(err.to_string().contains("corrupt authoritative event"));
    }
}
