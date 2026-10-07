//! A session's past turns, replayed for a client that reopens it.
//!
//! The snapshot carries the conversation's text only. What a live client also
//! saw — tool rows and how they ended, children, verification, the turn's
//! terminal — lives in the durable event log. This projects that log through
//! the same [`EventBridge`] a live client received, so a reopened session reads
//! the way it did while it ran instead of a second, drifting dialect.

use std::collections::HashMap;

use leveler_client_protocol::{MessageId, RuntimeEvent, UiHistoryEntry, UiMessage, UiRole};
use leveler_core::{SessionId, Timestamp, TurnId};
use leveler_engine::EngineEvent;
use leveler_model::{ContentPart, Message, TranscriptOrigin};
use leveler_storage::{Database, EventStore, MessageRepository, TurnRecord, TurnRepository};
use tokio::sync::broadcast;

use crate::AppError;
use crate::event_bridge::EventBridge;

/// The most recent turns a history carries; older turns are counted, not sent.
pub const HISTORY_TURNS_MAX: usize = 100;

/// One durable fact in time order: an engine event or a message the user sent.
enum Fact {
    Event(Box<EngineEvent>),
    /// A user message: its text, and how many images it carried.
    User(String, usize),
    /// A host-marked incomplete raw response; identity is its transcript row.
    PartialAssistant(MessageId, String),
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
    // The message store is the ACTIVE MODEL CONTEXT, and `/compact` replaced
    // the turns before its cut with a single summary row. The conversation is
    // not the context: the user asked for a smaller next request, not for
    // their own words to leave a session they reopen. The accepted input of
    // every turn is still durable in its write-ahead turn row, which no epoch
    // cut touches, so re-project it here.
    let rebuild = rebuildable_turn_inputs(&events, db, session_id).await?;
    let messages: Vec<(Option<Timestamp>, Fact)> = MessageRepository::new(db)
        .load_timed(session_id)
        .await
        .map_err(AppError::from)?
        .into_iter()
        .filter_map(|m| {
            transcript_fact(
                &m.payload,
                MessageId::new(format!("{session_id}-partial-{}", m.ordinal)),
            )
            .map(|fact| (parse_time(&m.created_at), fact))
        })
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
            facts.push(messages.next().expect("peeked"));
        }
        // The input that opened a rebuilt turn takes the turn's own place in
        // the chronology: the durable `TurnStarted` names the turn, and the
        // turn row holds what the user accepted for it.
        let rebuilt = match &event {
            EngineEvent::TurnStarted { turn_id, .. } => rebuild.get(turn_id).cloned(),
            _ => None,
        };
        facts.push((at, Fact::Event(Box::new(event))));
        if let Some(message) = rebuilt {
            let (text, images) = user_input(&message);
            facts.push((at, Fact::User(text, images)));
        }
    }
    facts.extend(messages);

    // A turn is everything from one TurnStarted to the next.
    //
    // A context epoch cut (`/compact`) is not part of the turn it follows: it
    // is written once that turn has already settled. Each group is replayed
    // through a bridge that publishes exactly one terminal and drops whatever
    // comes after it, so a compaction left in the settled group vanished from
    // every rebuilt conversation. It opens its own group instead — which is
    // what it is: the start of a new context epoch.
    let mut turns: Vec<Vec<(Option<Timestamp>, Fact)>> = vec![Vec::new()];
    let mut settled = false;
    for (at, fact) in facts {
        let opens_turn =
            matches!(&fact, Fact::Event(e) if matches!(**e, EngineEvent::TurnStarted { .. }));
        let after_a_settled_turn = settled
            && matches!(&fact, Fact::Event(e) if matches!(**e, EngineEvent::Compacted { .. }));
        if (opens_turn || after_a_settled_turn) && !turns.last().is_some_and(Vec::is_empty) {
            turns.push(Vec::new());
            settled = false;
        }
        if matches!(&fact, Fact::Event(e) if matches!(**e, EngineEvent::TaskFinished { .. })) {
            settled = true;
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
            Fact::User(text, images) => vec![RuntimeEvent::UserMessageAdded {
                message: UiMessage {
                    id: MessageId::new(leveler_core::new_uuid_string()),
                    role: UiRole::User,
                    text,
                    ordinal: None,
                    kind: None,
                    images,
                },
            }],
            Fact::PartialAssistant(id, text) => vec![
                RuntimeEvent::AssistantMessageStarted {
                    message_id: id.clone(),
                },
                RuntimeEvent::AssistantTextDelta {
                    message_id: id.clone(),
                    delta: text,
                },
                // Close only this persisted display block. This does not
                // complete the turn or convert incomplete output into an answer.
                RuntimeEvent::AssistantMessageCompleted { message_id: id },
            ],
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
            | RuntimeEvent::Notification { .. }
    )
}

/// Completed assistant replies are already represented by canonical events.
/// Interrupted replies have only a raw row: replay that row as its own
/// display block, leaving the authoritative turn terminal unchanged. The host-owned JSON boolean,
/// never a phrase in the body, distinguishes this path from normal messages.
fn transcript_fact(payload: &str, id: MessageId) -> Option<Fact> {
    if let Some((text, images)) = user_text(payload) {
        return Some(Fact::User(text, images));
    }
    let value: serde_json::Value = serde_json::from_str(payload).ok()?;
    if value.get("incomplete").and_then(serde_json::Value::as_bool) != Some(true) {
        return None;
    }
    let message: leveler_model::Message = serde_json::from_value(value).ok()?;
    if message.role != leveler_model::Role::Assistant {
        return None;
    }
    let text = message.text_content();
    (!text.is_empty()).then_some(Fact::PartialAssistant(id, text))
}

/// The text of a message the user sent. Runtime notices and compaction
/// summaries are user-role context the runtime wrote; the events already
/// show what they describe.
fn user_text(payload: &str) -> Option<(String, usize)> {
    let message: leveler_model::Message = serde_json::from_str(payload).ok()?;
    if message.role != leveler_model::Role::User {
        return None;
    }
    // The user transport role is how the model receives a protocol repair.
    // Origin, not the role and not the sentence, says the person did not write it.
    if message.is_protocol_repair() {
        return None;
    }
    let (text, images) = user_input(&message);
    let first = text.lines().next().unwrap_or("").trim_end();
    // A message that was only a picture has no text; replaying it as nothing
    // is how a reopened session lost the turn.
    if (text.trim().is_empty() && images == 0)
        || text.starts_with(leveler_client_protocol::COMPACTION_SUMMARY_PREFIX)
        || leveler_agent::RUNTIME_NOTICE_HEADERS.contains(&first)
    {
        return None;
    }
    Some((text, images))
}

/// What a user-authored message carries: its text and how many images.
fn user_input(message: &Message) -> (String, usize) {
    let images = message
        .content
        .iter()
        .filter(|part| matches!(part, ContentPart::Image { .. }))
        .count();
    (message.text_content(), images)
}

/// Which positions in the durable log a reopen rebuilds from the write-ahead
/// turn rows instead of the message store.
///
/// Two epoch cuts replace the transcript, and they mean opposite things:
///
/// - `/compact` installs the compaction summary as the model-visible context.
///   The conversation is re-based on it, so the turns before the cut keep
///   their place in the conversation even though their transcript rows are
///   gone — this is the window a reopen rebuilds.
/// - `/clear` (and a restore all the way back to the start) installs an EMPTY
///   context at transcript watermark zero. The user told us to start over, so
///   the turns that cut removed never come back, however durable their turn
///   rows are.
///
/// A checkpoint restore to a real position installs an empty context at a
/// non-zero watermark: the transcript keeps the prefix it rolled back to and
/// stays the authority, so it is neither a re-base nor a drop.
#[derive(Default)]
struct EpochWindow {
    /// The last `/compact` that re-based the visible conversation.
    rebased_at: Option<usize>,
    /// The last cut that emptied the conversation outright.
    dropped_at: Option<usize>,
}

impl EpochWindow {
    fn of<'a>(events: impl Iterator<Item = &'a EngineEvent>) -> Self {
        let mut window = Self::default();
        for (index, event) in events.enumerate() {
            let EngineEvent::ContextSnapshot {
                messages,
                through_ordinal: Some(watermark),
            } = event
            else {
                continue;
            };
            if messages
                .iter()
                .any(|message| message.origin == Some(TranscriptOrigin::CompactionSummary))
            {
                window.rebased_at = Some(index);
            } else if messages.is_empty() && *watermark == 0 {
                window.dropped_at = Some(index);
            }
        }
        window
    }

    /// Is the turn at `at` inside the conversation the last re-base left
    /// behind? Turns before a re-base lost their rows to it; turns before a
    /// `/clear` are gone by the user's own instruction.
    fn rebuilds(&self, at: usize) -> bool {
        self.rebased_at.is_some_and(|rebased| at < rebased)
            && self.dropped_at.is_none_or(|dropped| at > dropped)
    }
}

/// The accepted input of every turn the message store no longer carries, keyed
/// by the turn that accepted it. Empty when the store still holds the visible
/// conversation — the common case, and the only one where nothing is rebuilt.
async fn rebuildable_turn_inputs(
    events: &[(Option<Timestamp>, EngineEvent)],
    db: &Database,
    session_id: &SessionId,
) -> Result<HashMap<TurnId, Message>, AppError> {
    let window = EpochWindow::of(events.iter().map(|(_, event)| event));
    if window.rebased_at.is_none() {
        return Ok(HashMap::new());
    }
    let turns = TurnRepository::new(db)
        .list(session_id)
        .await
        .map_err(AppError::from)?;
    let inputs: HashMap<&str, &TurnRecord> = turns
        .iter()
        .filter(|turn| matches!(turn.kind.as_str(), "user" | "chat"))
        .map(|turn| (turn.id.as_str(), turn))
        .collect();
    let mut rebuild = HashMap::new();
    for (index, (_, event)) in events.iter().enumerate() {
        let EngineEvent::TurnStarted { turn_id, .. } = event else {
            continue;
        };
        if !window.rebuilds(index) {
            continue;
        }
        let Some(payload) = inputs
            .get(turn_id.as_str())
            .and_then(|turn| turn.payload.as_deref())
        else {
            continue;
        };
        // A fresh turn always wrote a versioned write-ahead input, so a
        // payload this boundary cannot read is corruption of the accepted
        // request — never silently a turn without its question.
        let message = decode_input(payload, session_id, turn_id)?;
        if let Some(message) = message {
            rebuild.insert(turn_id.clone(), message);
        }
    }
    Ok(rebuild)
}

/// The turn's accepted input, or `None` for a continuation that legitimately
/// carries no new message.
fn decode_input(
    payload: &str,
    session_id: &SessionId,
    turn_id: &TurnId,
) -> Result<Option<Message>, AppError> {
    leveler_engine::decode_turn_initiating_message_opt(payload).map_err(|error| {
        AppError::Engine(format!(
            "corrupt turn input: session {} turn {}: {error}",
            session_id.as_str(),
            turn_id.as_str()
        ))
    })
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
    use leveler_lifecycle::{StopReason, TaskOutcome};
    use leveler_model::{Message, ProtocolRepairKind, Role, TranscriptOrigin};
    use leveler_storage::{
        EventRepository, EventStore, MessageRepository, SessionRecord, SessionRepository,
        TurnRepository,
    };

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
                reasoning: Vec::new(),
            },
        )
        .await;
        event(
            db,
            sid,
            t0 + ms(1500),
            EngineEvent::TaskFinished {
                outcome: TaskOutcome::Completed,
                reason: None,
                failure: None,
                stop: Some(StopReason::Answered),
                warnings: Vec::new(),
            },
        )
        .await;
    }

    #[tokio::test]
    async fn interrupted_raw_body_reopens_once_without_promoting_user_markers() {
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
        user(
            &db,
            &sid,
            t0 + ms(1),
            "[Response interrupted before completion.] user text",
        )
        .await;
        let complete = serde_json::to_string(&leveler_model::Message::text(
            leveler_model::Role::Assistant,
            "complete earlier answer",
        ))
        .unwrap();
        MessageRepository::new(&db)
            .append(&sid, &[complete], t0 + ms(2))
            .await
            .unwrap();
        event(
            &db,
            &sid,
            t0 + ms(2),
            EngineEvent::AssistantMessage {
                text: "complete earlier answer".into(),
                reasoning: Vec::new(),
            },
        )
        .await;
        let partial = serde_json::json!({"role":"assistant","content":[{"type":"text","text":"observed partial answer"}],"incomplete":true}).to_string();
        MessageRepository::new(&db)
            .append(&sid, &[partial], t0 + ms(3))
            .await
            .unwrap();
        event(
            &db,
            &sid,
            t0 + ms(4),
            EngineEvent::AssistantMessage {
                text: "later repaired answer".into(),
                reasoning: Vec::new(),
            },
        )
        .await;
        event(
            &db,
            &sid,
            t0 + ms(5),
            EngineEvent::TaskFinished {
                outcome: TaskOutcome::Interrupted,
                reason: Some("cancelled".into()),
                failure: None,
                stop: None,
                warnings: vec![],
            },
        )
        .await;
        for _ in 0..2 {
            let (history, _) = load_session_history(&db, &sid).await.unwrap();
            let text: Vec<_> = history
                .iter()
                .filter_map(|entry| match &entry.event {
                    RuntimeEvent::AssistantTextDelta { delta, .. } => Some(delta.as_str()),
                    _ => None,
                })
                .collect();
            assert_eq!(
                text,
                vec![
                    "complete earlier answer",
                    "observed partial answer",
                    "later repaired answer"
                ],
                "canonical completion and partial raw body each appear once on reopen"
            );
            assert!(
                !history
                    .iter()
                    .any(|entry| matches!(entry.event, RuntimeEvent::TurnCompleted)),
                "cancelled history cannot become a completed turn"
            );
        }
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

    /// REASONING-4 / REASONING-9: a reopened session restores the provider's
    /// reasoning as completed Thought segments, in order, before the answer
    /// text. The durable assistant-message projection carries it; the message
    /// store stays the model-context authority. More than one segment is
    /// preserved as more than one Thought — never collapsed into one duration.
    #[tokio::test]
    async fn resume_restores_completed_thoughts_before_the_answer() {
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
        user(&db, &sid, t0 + ms(10), "看下最新模型").await;
        event(
            &db,
            &sid,
            t0 + ms(1000),
            EngineEvent::AssistantMessage {
                text: "已同步。".into(),
                reasoning: vec![
                    leveler_model::ReasoningSegment {
                        text: "先查 catalog。".into(),
                        duration_ms: 1600,
                    },
                    leveler_model::ReasoningSegment {
                        text: "再补一条失败测试。".into(),
                        duration_ms: 900,
                    },
                ],
            },
        )
        .await;
        event(
            &db,
            &sid,
            t0 + ms(1500),
            EngineEvent::TaskFinished {
                outcome: TaskOutcome::Completed,
                reason: None,
                failure: None,
                stop: Some(StopReason::Answered),
                warnings: Vec::new(),
            },
        )
        .await;

        let (history, _) = load_session_history(&db, &sid).await.unwrap();
        let shaped: Vec<(&'static str, String)> = history
            .iter()
            .filter_map(|entry| match &entry.event {
                RuntimeEvent::ReasoningStarted => Some(("started", String::new())),
                RuntimeEvent::ReasoningDelta { delta } => Some(("delta", delta.clone())),
                RuntimeEvent::ReasoningCompleted { elapsed_ms } => {
                    Some(("completed", elapsed_ms.to_string()))
                }
                RuntimeEvent::AssistantTextDelta { delta, .. } => Some(("text", delta.clone())),
                _ => None,
            })
            .collect();
        assert_eq!(
            shaped,
            vec![
                ("started", String::new()),
                ("delta", "先查 catalog。".into()),
                ("completed", "1600".into()),
                ("started", String::new()),
                ("delta", "再补一条失败测试。".into()),
                ("completed", "900".into()),
                ("text", "已同步。".into()),
            ],
            "each durable segment replays as its own Thought, before the answer"
        );
        // The reasoning is a UI projection, not a turn-shape change.
        assert_eq!(
            tags(&history).last().map(String::as_str),
            Some("turn_answered")
        );
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
                model_step: None,
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
                reasoning: Vec::new(),
            },
        )
        .await;
        event(
            &db,
            &sid,
            t0 + ms(6000),
            EngineEvent::TaskFinished {
                outcome: TaskOutcome::Completed,
                reason: None,
                failure: None,
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

    /// The sentence the harness persists for a quiet goal. Kept here so the
    /// history test does not depend on the private nudge constructor.
    const GOAL_CLOSEOUT: &str = "Goal remains active. Continue working toward the original goal, \
         and resolve it with update_goal(complete|blocked) when the work is finished or cannot proceed.";

    fn user_texts(entries: &[UiHistoryEntry]) -> Vec<String> {
        entries
            .iter()
            .filter_map(|entry| match &entry.event {
                RuntimeEvent::UserMessageAdded { message } => Some(message.text.clone()),
                _ => None,
            })
            .collect()
    }

    async fn message_row(db: &Database, sid: &SessionId, at: Timestamp, message: &Message) {
        let payload = serde_json::to_string(message).unwrap();
        MessageRepository::new(db)
            .append(sid, &[payload], at)
            .await
            .unwrap();
    }

    /// A goal closeout row stays in the model transcript. Replay must not
    /// present it as something the user wrote. Real user English, including
    /// text that happens to mention the same words, still replays.
    #[tokio::test]
    async fn a_goal_closeout_injection_is_model_context_not_user_history() {
        let (db, sid) = session().await;
        let t0 = now();
        event(
            &db,
            &sid,
            t0,
            EngineEvent::TurnStarted {
                turn_id: TurnId::generate(),
                kind: TurnKind::User,
            },
        )
        .await;
        user(&db, &sid, t0 + ms(10), "修一下解析器").await;
        message_row(
            &db,
            &sid,
            t0 + ms(20),
            &Message::user_input("Goal remains active. I wrote this myself."),
        )
        .await;
        user(&db, &sid, t0 + ms(30), "please continue").await;
        message_row(
            &db,
            &sid,
            t0 + ms(40),
            &Message::user(
                GOAL_CLOSEOUT,
                TranscriptOrigin::ProtocolRepair {
                    repair: ProtocolRepairKind::GoalUnresolved,
                },
            ),
        )
        .await;
        event(
            &db,
            &sid,
            t0 + ms(1000),
            EngineEvent::AssistantMessage {
                text: "解析器已修好。".into(),
                reasoning: Vec::new(),
            },
        )
        .await;
        event(
            &db,
            &sid,
            t0 + ms(1500),
            EngineEvent::TaskFinished {
                outcome: TaskOutcome::Completed,
                reason: None,
                failure: None,
                stop: Some(StopReason::Answered),
                warnings: Vec::new(),
            },
        )
        .await;

        let (entries, _) = load_session_history(&db, &sid).await.unwrap();
        assert_eq!(
            user_texts(&entries),
            vec![
                "修一下解析器".to_string(),
                "Goal remains active. I wrote this myself.".to_string(),
                "please continue".to_string(),
            ],
            "protocol repair must not join the user-visible history: {entries:#?}"
        );

        let rows = MessageRepository::new(&db).load(&sid).await.unwrap();
        assert_eq!(rows.len(), 4, "the model still needs the persisted nudge");
        let nudge: Message = serde_json::from_str(
            rows.iter()
                .find(|payload| payload.contains("update_goal(complete|blocked)"))
                .expect("closeout row"),
        )
        .unwrap();
        assert!(matches!(
            nudge.origin,
            Some(TranscriptOrigin::ProtocolRepair {
                repair: ProtocolRepairKind::GoalUnresolved
            })
        ));
        assert_eq!(nudge.role, Role::User);
    }

    /// A chat turn is not a goal closeout. User text replays even when it
    /// contains the words the harness also uses.
    #[tokio::test]
    async fn a_chat_turn_replays_user_text_that_mentions_an_active_goal() {
        let (db, sid) = session().await;
        let t0 = now();
        turn(&db, &sid, t0, "Goal remains active in my notes", "好的").await;
        let (entries, _) = load_session_history(&db, &sid).await.unwrap();
        assert_eq!(
            user_texts(&entries),
            vec!["Goal remains active in my notes".to_string()]
        );
    }

    // ---- compaction / epoch-cut resume (COMPACT-RESUME-*) -------------------

    /// A real turn: the durable write-ahead row FIRST (it is what makes the
    /// turn running and is the accepted request), then the turn's transcript
    /// row, then the canonical events. Same order the engine writes them.
    async fn durable_turn(
        db: &Database,
        sid: &SessionId,
        t0: Timestamp,
        ask: &str,
        answer: &str,
    ) -> TurnId {
        let payload = serde_json::json!({
            "version": 1,
            "initiating_message": Message::text(Role::User, ask),
        })
        .to_string();
        let record = TurnRepository::new(db)
            .start(sid, "chat", Some(&payload), t0)
            .await
            .unwrap();
        let turn_id = TurnId::new(record.id.clone());
        event(
            db,
            sid,
            t0,
            EngineEvent::TurnStarted {
                turn_id: turn_id.clone(),
                kind: TurnKind::Chat,
            },
        )
        .await;
        let body = serde_json::to_string(&Message::text(Role::User, ask)).unwrap();
        MessageRepository::new(db)
            .append_in_turn(sid, &turn_id, &[body], t0 + ms(10))
            .await
            .unwrap();
        event(
            db,
            sid,
            t0 + ms(1000),
            EngineEvent::AssistantMessage {
                text: answer.into(),
                reasoning: Vec::new(),
            },
        )
        .await;
        event(
            db,
            sid,
            t0 + ms(1500),
            EngineEvent::TaskFinished {
                outcome: TaskOutcome::Completed,
                reason: None,
                failure: None,
                stop: Some(StopReason::Answered),
                warnings: Vec::new(),
            },
        )
        .await;
        TurnRepository::new(db)
            .finish(&turn_id, "completed", t0 + ms(1500))
            .await
            .unwrap();
        turn_id
    }

    fn whole_history_summary() -> Message {
        Message::user(
            format!(
                "{}：\n前面的话题已折叠。",
                leveler_client_protocol::COMPACTION_SUMMARY_PREFIX
            ),
            TranscriptOrigin::CompactionSummary,
        )
    }

    /// The manual `/compact` transaction: the summary replaces the whole
    /// transcript and the epoch events land in the same commit.
    async fn compact(db: &Database, sid: &SessionId, from: usize, at: Timestamp) {
        let summary = serde_json::to_string(&whole_history_summary()).unwrap();
        let rows = epoch_rows(from, vec![whole_history_summary()], 1);
        db.cut_context_epoch(sid, &[summary], &rows, at)
            .await
            .unwrap();
    }

    /// The epoch events `/compact` commits, in their canonical order.
    fn epoch_rows(from: usize, model_visible: Vec<Message>, through: u64) -> Vec<(String, String)> {
        let mut rows = vec![EngineEvent::Compacted {
            from,
            to: model_visible.len(),
        }];
        rows.push(EngineEvent::ContextSnapshot {
            messages: model_visible,
            through_ordinal: Some(through),
        });
        rows.into_iter()
            .map(|event| event.to_row().unwrap())
            .collect()
    }

    /// `/clear`: the transcript is emptied and the epoch snapshot installs an
    /// empty model context at watermark zero.
    async fn clear(db: &Database, sid: &SessionId, at: Timestamp) {
        MessageRepository::new(db)
            .truncate_after(sid, 0)
            .await
            .unwrap();
        let (tag, payload) = EngineEvent::ContextSnapshot {
            messages: Vec::new(),
            through_ordinal: Some(0),
        }
        .to_row()
        .unwrap();
        EventRepository::new(db)
            .append(sid, None, &tag, &payload, at)
            .await
            .unwrap();
    }

    /// COMPACT-RESUME-1: `/compact` changes what the next provider request
    /// carries. It must not erase the user's own words from a session they
    /// reopen — the accepted input of every turn survives in its turn row.
    #[tokio::test]
    async fn compaction_keeps_the_first_user_turn_on_a_reopen() {
        let (db, sid) = session().await;
        let t0 = now();
        durable_turn(
            &db,
            &sid,
            t0,
            "第一个问题：入口在哪？",
            "在 crates/leveler-cli。",
        )
        .await;
        durable_turn(
            &db,
            &sid,
            t0 + ms(10_000),
            "第二个问题：测试怎么跑？",
            "用 cargo test。",
        )
        .await;
        compact(&db, &sid, 4, t0 + ms(20_000)).await;
        durable_turn(
            &db,
            &sid,
            t0 + ms(30_000),
            "压缩后继续：还有别的吗？",
            "没有了。",
        )
        .await;

        let (entries, _) = load_session_history(&db, &sid).await.unwrap();
        assert_eq!(
            user_texts(&entries),
            vec![
                "第一个问题：入口在哪？".to_string(),
                "第二个问题：测试怎么跑？".to_string(),
                "压缩后继续：还有别的吗？".to_string(),
            ],
            "the reopened conversation keeps its head and its order: {entries:#?}"
        );
        assert_eq!(
            entries
                .iter()
                .filter(|entry| entry.turn_start
                    && matches!(entry.event, RuntimeEvent::UserMessageAdded { .. }))
                .count(),
            3,
            "each rebuilt turn opens its own turn at its input: {entries:#?}"
        );
        assert!(
            user_texts(&entries)
                .iter()
                .all(|text| !text.contains(leveler_client_protocol::COMPACTION_SUMMARY_PREFIX)),
            "the internal summary is a context artifact, not a conversation row"
        );
    }

    /// COMPACT-RESUME-2: compaction twice. A rebuilt turn must not be
    /// projected on top of a transcript row that still exists.
    #[tokio::test]
    async fn two_compactions_do_not_duplicate_a_user_turn() {
        let (db, sid) = session().await;
        let t0 = now();
        durable_turn(&db, &sid, t0, "一问", "一答").await;
        compact(&db, &sid, 2, t0 + ms(10_000)).await;
        durable_turn(&db, &sid, t0 + ms(20_000), "二问", "二答").await;
        compact(&db, &sid, 4, t0 + ms(30_000)).await;
        durable_turn(&db, &sid, t0 + ms(40_000), "三问", "三答").await;

        let (entries, _) = load_session_history(&db, &sid).await.unwrap();
        assert_eq!(
            user_texts(&entries),
            vec!["一问".to_string(), "二问".to_string(), "三问".to_string()],
            "{entries:#?}"
        );
    }

    /// COMPACT-RESUME-3: rebuilding the transcript is a READ projection. The
    /// model's request surface stays exactly what the compaction left.
    #[tokio::test]
    async fn a_reopen_does_not_widen_the_provider_request_surface() {
        let (db, sid) = session().await;
        let t0 = now();
        durable_turn(&db, &sid, t0, "一问", "一答").await;
        durable_turn(&db, &sid, t0 + ms(10_000), "二问", "二答").await;
        compact(&db, &sid, 4, t0 + ms(20_000)).await;
        let after_compact = MessageRepository::new(&db).load(&sid).await.unwrap();

        let (entries, _) = load_session_history(&db, &sid).await.unwrap();
        assert_eq!(user_texts(&entries).len(), 2, "the conversation is rebuilt");

        let after_replay = MessageRepository::new(&db).load(&sid).await.unwrap();
        assert_eq!(
            after_replay, after_compact,
            "replaying history must not write anything back into the model context"
        );
        assert_eq!(
            after_replay.len(),
            1,
            "the summary is the whole request surface"
        );
        // And nothing was appended to the log either: the projection is derived.
        let events = EventRepository::new(&db).load(&sid).await.unwrap();
        assert_eq!(
            events
                .iter()
                .filter(|row| row.event_type == "user_message_added")
                .count(),
            0
        );
    }

    /// The opposite instruction: `/clear` empties the conversation, so the
    /// turns it removed stay gone even though their turn rows are durable.
    #[tokio::test]
    async fn a_cleared_conversation_does_not_come_back() {
        let (db, sid) = session().await;
        let t0 = now();
        durable_turn(&db, &sid, t0, "清空前的追问", "清空前的回答").await;
        clear(&db, &sid, t0 + ms(10_000)).await;
        durable_turn(&db, &sid, t0 + ms(20_000), "清空后的追问", "清空后的回答").await;

        let (entries, _) = load_session_history(&db, &sid).await.unwrap();
        assert_eq!(
            user_texts(&entries),
            vec!["清空后的追问".to_string()],
            "{entries:#?}"
        );
    }

    /// A checkpoint restore keeps the prefix it rolled back to. Its transcript
    /// rows are still the authority, so a rebuilt row must not double it.
    #[tokio::test]
    async fn a_restore_of_the_prefix_does_not_duplicate_a_user_turn() {
        let (db, sid) = session().await;
        let t0 = now();
        durable_turn(&db, &sid, t0, "保留的一问", "保留的一答").await;
        // Roll back to the first message: the prefix survives, the snapshot
        // records the watermark it superseded.
        MessageRepository::new(&db)
            .truncate_after(&sid, 1)
            .await
            .unwrap();
        let (tag, payload) = EngineEvent::ContextSnapshot {
            messages: Vec::new(),
            through_ordinal: Some(1),
        }
        .to_row()
        .unwrap();
        EventRepository::new(&db)
            .append(&sid, None, &tag, &payload, t0 + ms(10_000))
            .await
            .unwrap();

        let (entries, _) = load_session_history(&db, &sid).await.unwrap();
        assert_eq!(
            user_texts(&entries),
            vec!["保留的一问".to_string()],
            "{entries:#?}"
        );
    }

    /// A compaction that happened inside a running turn (the automatic fold)
    /// never replaced the transcript, so its turn keeps the single row the
    /// message store already holds.
    #[tokio::test]
    async fn an_in_loop_compaction_does_not_double_a_user_turn() {
        let (db, sid) = session().await;
        let t0 = now();
        let ask = "在跑的回合里折叠上下文";
        let payload = serde_json::json!({
            "version": 1,
            "initiating_message": Message::text(Role::User, ask),
        })
        .to_string();
        let record = TurnRepository::new(&db)
            .start(&sid, "chat", Some(&payload), t0)
            .await
            .unwrap();
        let turn_id = TurnId::new(record.id.clone());
        event(
            &db,
            &sid,
            t0,
            EngineEvent::TurnStarted {
                turn_id: turn_id.clone(),
                kind: TurnKind::Chat,
            },
        )
        .await;
        let body = serde_json::to_string(&Message::text(Role::User, ask)).unwrap();
        MessageRepository::new(&db)
            .append_in_turn(&sid, &turn_id, &[body], t0 + ms(10))
            .await
            .unwrap();
        event(
            &db,
            &sid,
            t0 + ms(500),
            EngineEvent::Compacted { from: 30, to: 3 },
        )
        .await;
        event(
            &db,
            &sid,
            t0 + ms(600),
            EngineEvent::ContextSnapshot {
                messages: vec![whole_history_summary()],
                through_ordinal: None,
            },
        )
        .await;
        event(
            &db,
            &sid,
            t0 + ms(1500),
            EngineEvent::TaskFinished {
                outcome: TaskOutcome::Completed,
                reason: None,
                failure: None,
                stop: Some(StopReason::Answered),
                warnings: Vec::new(),
            },
        )
        .await;

        let (entries, _) = load_session_history(&db, &sid).await.unwrap();
        assert_eq!(user_texts(&entries), vec![ask.to_string()], "{entries:#?}");
    }
}
