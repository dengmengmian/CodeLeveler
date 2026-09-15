//! A session's past turns, replayed for a client that reopens it.
//!
//! The snapshot carries the conversation's text only. What a live client also
//! saw — tool rows and how they ended, children, verification, the turn's
//! terminal — lives in the durable event log. This projects that log through
//! the same [`EventBridge`] a live client received, so a reopened session reads
//! the way it did while it ran instead of a second, drifting dialect.

use std::collections::HashMap;

use leveler_client_protocol::{MessageId, RuntimeEvent, UiHistoryEntry, UiMessage, UiRole};
use leveler_core::{SessionId, Timestamp};
use leveler_engine::EngineEvent;
use leveler_storage::{Database, EventStore, MessageRepository};
use tokio::sync::broadcast;

use crate::AppError;
use crate::event_bridge::EventBridge;

/// The most recent turns a history carries; older turns are counted, not sent.
pub const HISTORY_TURNS_MAX: usize = 100;

/// One durable fact in time order: an engine event or a message the user sent.
enum Fact {
    Event(Box<EngineEvent>),
    User(String),
}

/// The latest [`HISTORY_TURNS_MAX`] turns of a session, and how many older
/// turns were left out. Empty when the session has no durable turn at all —
/// a client then keeps the snapshot's view.
pub async fn load_session_history(
    db: &Database,
    session_id: &SessionId,
) -> Result<(Vec<UiHistoryEntry>, u32), AppError> {
    let records = db.load(session_id).await.map_err(AppError::from)?;
    let mut events = Vec::with_capacity(records.len());
    for record in &records {
        let event = EngineEvent::from_payload(&record.payload).map_err(|e| {
            AppError::Engine(format!(
                "corrupt authoritative event: session {} sequence {} type '{}': {e}",
                record.session_id, record.sequence, record.event_type
            ))
        })?;
        events.push((parse_time(&record.created_at), event));
    }
    if !events
        .iter()
        .any(|(_, e)| matches!(e, EngineEvent::TurnStarted { .. }))
    {
        return Ok((Vec::new(), 0));
    }
    let messages: Vec<(Option<Timestamp>, String)> = MessageRepository::new(db)
        .load_timed(session_id)
        .await
        .map_err(AppError::from)?
        .into_iter()
        .filter_map(|m| user_text(&m.payload).map(|text| (parse_time(&m.created_at), text)))
        .collect();

    // Both logs are already ordered; merge them by record time without
    // reordering either one.
    let mut facts: Vec<(Option<Timestamp>, Fact)> =
        Vec::with_capacity(events.len() + messages.len());
    let mut messages = messages.into_iter().peekable();
    for (at, event) in events {
        while let Some((message_at, _)) = messages.peek() {
            if !matches!((message_at, at), (Some(m), Some(e)) if *m <= e) {
                break;
            }
            let (message_at, text) = messages.next().expect("peeked");
            facts.push((message_at, Fact::User(text)));
        }
        facts.push((at, Fact::Event(Box::new(event))));
    }
    facts.extend(messages.map(|(at, text)| (at, Fact::User(text))));

    // A turn is everything from one TurnStarted to the next.
    let mut turns: Vec<Vec<(Option<Timestamp>, Fact)>> = vec![Vec::new()];
    for (at, fact) in facts {
        if matches!(&fact, Fact::Event(e) if matches!(**e, EngineEvent::TurnStarted { .. }))
            && !turns.last().is_some_and(Vec::is_empty)
        {
            turns.push(Vec::new());
        }
        turns.last_mut().expect("one turn").push((at, fact));
    }
    let omitted = turns.len().saturating_sub(HISTORY_TURNS_MAX);

    let mut entries = Vec::new();
    for turn in turns.into_iter().skip(omitted) {
        replay_turn(turn, &mut entries);
    }
    Ok((entries, omitted as u32))
}

/// Project one turn through a fresh bridge — each turn has its own terminal,
/// and a bridge publishes exactly one.
fn replay_turn(turn: Vec<(Option<Timestamp>, Fact)>, entries: &mut Vec<UiHistoryEntry>) {
    let (sender, mut receiver) = broadcast::channel(256);
    let mut bridge = EventBridge::new(sender);
    let started = turn.iter().find_map(|(at, _)| *at);
    let mut tool_starts: HashMap<String, Timestamp> = HashMap::new();
    let mut first = true;
    for (at, fact) in turn {
        let elapsed_ms = match (started, at) {
            (Some(start), Some(at)) => (at - start).num_milliseconds().max(0) as u64,
            _ => 0,
        };
        let produced = match fact {
            Fact::User(text) => vec![RuntimeEvent::UserMessageAdded {
                message: UiMessage {
                    id: MessageId::new(leveler_core::new_uuid_string()),
                    role: UiRole::User,
                    text,
                    ordinal: None,
                    kind: None,
                },
            }],
            Fact::Event(event) => {
                if let (
                    EngineEvent::ToolCallStarted {
                        call_id,
                        agent_id: None,
                        ..
                    },
                    Some(at),
                ) = (&*event, at)
                {
                    tool_starts.insert(call_id.clone(), at);
                }
                bridge.forward(*event);
                let mut out = Vec::new();
                while let Ok(event) = receiver.try_recv() {
                    out.push(event);
                }
                out
            }
        };
        for mut event in produced {
            if is_live_only(&event) {
                continue;
            }
            // The bridge times a call with a live clock; the durable record
            // times are the only clock a replay has.
            if let RuntimeEvent::ToolCallCompleted {
                id, duration_ms, ..
            } = &mut event
            {
                *duration_ms = match (tool_starts.get(id.as_str()), at) {
                    (Some(start), Some(end)) => (end - *start).num_milliseconds().max(0) as u64,
                    _ => 0,
                };
            }
            entries.push(UiHistoryEntry {
                turn_elapsed_ms: elapsed_ms,
                turn_start: std::mem::take(&mut first),
                event,
            });
        }
    }
}

/// Status-line chrome that described a moment, not a record of the turn.
fn is_live_only(event: &RuntimeEvent) -> bool {
    matches!(
        event,
        RuntimeEvent::TurnFinalizing { .. }
            | RuntimeEvent::TurnProgress { .. }
            | RuntimeEvent::AgentActivity { .. }
            | RuntimeEvent::CommandProgress { .. }
            | RuntimeEvent::TokenUsage { .. }
            | RuntimeEvent::ReasoningDelta { .. }
            | RuntimeEvent::Notification { .. }
    )
}

/// The text of a message the user sent. Runtime notices and compaction
/// summaries are user-role context the runtime wrote; the events already
/// show what they describe.
fn user_text(payload: &str) -> Option<String> {
    let message: leveler_model::Message = serde_json::from_str(payload).ok()?;
    if message.role != leveler_model::Role::User {
        return None;
    }
    let text = message.text_content();
    let first = text.lines().next().unwrap_or("").trim_end();
    if text.trim().is_empty()
        || text.starts_with(leveler_client_protocol::COMPACTION_SUMMARY_PREFIX)
        || leveler_agent::RUNTIME_NOTICE_HEADERS.contains(&first)
    {
        return None;
    }
    Some(text)
}

/// A record time as written (RFC 3339); `None` when unreadable.
fn parse_time(text: &str) -> Option<Timestamp> {
    text.parse::<Timestamp>().ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use leveler_client_protocol::UiCommandStop;
    use leveler_core::{Timestamp, TurnId, now};
    use leveler_engine::{EngineEvent, TurnKind};
    use leveler_lifecycle::{StopReason, TaskOutcome, VerificationStatus};
    use leveler_model::{Message, Role};
    use leveler_storage::{EventStore, MessageRepository, SessionRecord, SessionRepository};

    async fn session() -> (Database, SessionId) {
        let db = Database::connect_in_memory().await.unwrap();
        let rec = SessionRecord::new("/repo", "history", "m/x", now());
        let sid = SessionId::new(rec.id.clone());
        SessionRepository::new(&db).create(&rec).await.unwrap();
        (db, sid)
    }

    async fn event(db: &Database, sid: &SessionId, at: Timestamp, ev: EngineEvent) {
        let (ty, payload) = ev.to_row().unwrap();
        db.append(sid, None, &ty, &payload, at).await.unwrap();
    }

    async fn user(db: &Database, sid: &SessionId, at: Timestamp, text: &str) {
        let payload = serde_json::to_string(&Message::text(Role::User, text)).unwrap();
        MessageRepository::new(db)
            .append(sid, &[payload], at)
            .await
            .unwrap();
    }

    fn ms(n: i64) -> std::time::Duration {
        std::time::Duration::from_millis(n as u64)
    }

    async fn turn(db: &Database, sid: &SessionId, t0: Timestamp, ask: &str, answer: &str) {
        event(
            db,
            sid,
            t0,
            EngineEvent::TurnStarted {
                turn_id: TurnId::generate(),
                kind: TurnKind::Chat,
            },
        )
        .await;
        user(db, sid, t0 + ms(10), ask).await;
        event(
            db,
            sid,
            t0 + ms(1000),
            EngineEvent::AssistantMessage {
                text: answer.into(),
            },
        )
        .await;
        event(
            db,
            sid,
            t0 + ms(1500),
            EngineEvent::TaskFinished {
                outcome: TaskOutcome::Completed,
                verification: VerificationStatus::NotRun,
                reason: None,
                stop: Some(StopReason::Answered),
                warnings: Vec::new(),
            },
        )
        .await;
    }

    fn tags(entries: &[UiHistoryEntry]) -> Vec<String> {
        entries
            .iter()
            .map(|e| {
                serde_json::to_value(&e.event).unwrap()["type"]
                    .as_str()
                    .unwrap()
                    .to_string()
            })
            .collect()
    }

    /// A stopped command reads as stopped after a reopen: the user's words,
    /// the tool row with its durable stop and real duration, the answer and
    /// the turn's terminal — in the order they happened.
    #[tokio::test]
    async fn a_turn_replays_its_messages_tools_and_terminal_in_order() {
        let (db, sid) = session().await;
        let t0 = now();
        event(
            &db,
            &sid,
            t0,
            EngineEvent::TurnStarted {
                turn_id: TurnId::generate(),
                kind: TurnKind::Chat,
            },
        )
        .await;
        user(&db, &sid, t0 + ms(10), "运行 soak，我会停掉它").await;
        event(
            &db,
            &sid,
            t0 + ms(1000),
            EngineEvent::ToolCallStarted {
                call_id: "c1".into(),
                name: "run_command".into(),
                arguments: r#"{"program":"./scripts/soak.sh"}"#.into(),
                parallel: false,
                risk: None,
                agent_id: None,
            },
        )
        .await;
        user(&db, &sid, t0 + ms(2000), "另外：告诉我停在哪").await;
        event(
            &db,
            &sid,
            t0 + ms(4000),
            EngineEvent::ToolCallFinished {
                call_id: "c1".into(),
                name: "run_command".into(),
                is_error: true,
                preview: "tool error: command was cancelled".into(),
                agent_id: None,
                applied_diff: None,
                exit_code: None,
                stop: Some(leveler_execution::CommandStop::Confirmed),
            },
        )
        .await;
        event(
            &db,
            &sid,
            t0 + ms(4500),
            EngineEvent::ProgressUpdated {
                ledger: Default::default(),
            },
        )
        .await;
        event(
            &db,
            &sid,
            t0 + ms(5000),
            EngineEvent::AssistantMessage {
                text: "停在第 3 个 tick。".into(),
            },
        )
        .await;
        event(
            &db,
            &sid,
            t0 + ms(6000),
            EngineEvent::TaskFinished {
                outcome: TaskOutcome::Completed,
                verification: VerificationStatus::NotRun,
                reason: None,
                stop: Some(StopReason::Answered),
                warnings: Vec::new(),
            },
        )
        .await;

        let (entries, omitted) = load_session_history(&db, &sid).await.unwrap();
        assert_eq!(omitted, 0);
        assert_eq!(
            tags(&entries),
            vec![
                "user_message_added",
                "tool_call_started",
                "user_message_added",
                "tool_call_completed",
                "assistant_message_started",
                "assistant_text_delta",
                "assistant_message_completed",
                "turn_answered",
            ],
            "{entries:#?}"
        );
        assert!(entries[0].turn_start);
        assert!(entries[1..].iter().all(|e| !e.turn_start));
        match &entries[3].event {
            RuntimeEvent::ToolCallCompleted {
                duration_ms, stop, ..
            } => {
                assert_eq!(*duration_ms, 3000);
                assert_eq!(*stop, Some(UiCommandStop::Confirmed));
            }
            other => panic!("{other:?}"),
        }
        assert_eq!(entries[3].turn_elapsed_ms, 4000);
        assert_eq!(entries.last().unwrap().turn_elapsed_ms, 6000);
    }

    /// Each turn starts its own entries, so every turn gets its terminal.
    #[tokio::test]
    async fn every_turn_replays_its_own_terminal() {
        let (db, sid) = session().await;
        let t0 = now();
        turn(&db, &sid, t0, "一", "答一").await;
        turn(&db, &sid, t0 + ms(60_000), "二", "答二").await;
        let (entries, _) = load_session_history(&db, &sid).await.unwrap();
        let t = tags(&entries);
        assert_eq!(
            t.iter().filter(|x| *x == "turn_answered").count(),
            2,
            "{t:?}"
        );
        assert_eq!(entries.iter().filter(|e| e.turn_start).count(), 2);
    }

    /// A long session sends its latest turns and says how many it left out.
    #[tokio::test]
    async fn a_long_history_keeps_the_latest_turns_and_counts_the_rest() {
        let (db, sid) = session().await;
        let t0 = now();
        for i in 0..(HISTORY_TURNS_MAX as i64 + 3) {
            turn(&db, &sid, t0 + ms(i * 10_000), &format!("问 {i}"), "答").await;
        }
        let (entries, omitted) = load_session_history(&db, &sid).await.unwrap();
        assert_eq!(omitted, 3);
        assert_eq!(
            entries.iter().filter(|e| e.turn_start).count(),
            HISTORY_TURNS_MAX
        );
        match &entries[0].event {
            RuntimeEvent::UserMessageAdded { message } => assert_eq!(message.text, "问 3"),
            other => panic!("{other:?}"),
        }
    }

    /// Nothing durable to replay: the client keeps the snapshot.
    #[tokio::test]
    async fn a_session_without_durable_turns_has_no_history() {
        let (db, sid) = session().await;
        user(&db, &sid, now(), "旧会话的一句话").await;
        let (entries, omitted) = load_session_history(&db, &sid).await.unwrap();
        assert!(entries.is_empty());
        assert_eq!(omitted, 0);
    }
}
