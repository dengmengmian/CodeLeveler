//! Recovery of turns left running by a boot that ended without finishing
//! them. Interrupting a turn is an authoritative write, so it requires a
//! current token — and it requires proof: a turn is settled only when its boot
//! is proven dead, or when this boot is settling its own work. A runtime
//! identity proves nothing here: several live boots share one.

use leveler_core::{BootId, BootLiveness, OwnershipToken, SessionId, TurnId};
use leveler_storage::{MessageStore, TerminalStore, TurnRecord, TurnStore};

use crate::{EngineError, EngineEvent, TaskEngine, TurnOutcome};

/// Reap the session's running turns with an ALREADY-HELD current token (the
/// engine's in-run path, before a new turn starts). The token is the proof:
/// acquiring it was refused while any running turn of the session had a boot
/// that was alive, unknown or unrecorded, and turns start only under the
/// current token — so every running turn left is this boot's own or belongs
/// to a boot proven dead. Each row transition commits atomically and fenced;
/// a commit failure propagates — a turn is never *assumed* interrupted.
pub async fn reap_running_turns_owned(
    turns: &dyn TurnStore,
    messages: &dyn MessageStore,
    terminal: &dyn TerminalStore,
    token: &OwnershipToken,
    session_id: Option<&SessionId>,
) -> Result<Vec<EngineEvent>, EngineError> {
    let running = turns.list_running(session_id).await?;
    interrupt_turns_owned(messages, terminal, token, &running).await
}

async fn interrupt_turns_owned(
    messages: &dyn MessageStore,
    terminal: &dyn TerminalStore,
    token: &OwnershipToken,
    running: &[TurnRecord],
) -> Result<Vec<EngineEvent>, EngineError> {
    let mut events = Vec::with_capacity(running.len());
    for turn in running {
        let session_id = SessionId::new(turn.session_id.clone());
        let turn_id = TurnId::new(turn.id.clone());
        // Fresh user/chat turns carry their initiating input in the same row
        // that made them `running`. If the process died before TurnSink
        // appended the transcript projection, rebuild it once by turn
        // identity before recording the interruption. Legacy/resume turns
        // have no such payload and retain their historical behavior.
        if matches!(turn.kind.as_str(), "user" | "chat")
            && let Some(payload) = turn.payload.as_deref()
        {
            let message = crate::turn::TurnInitiationPayload::decode(payload)?;
            let message_payload = serde_json::to_string(&message)?;
            messages
                .ensure_initiating_message_owned(
                    token,
                    &session_id,
                    &turn_id,
                    &message_payload,
                    leveler_core::now(),
                )
                .await?;
        }
        let event = EngineEvent::TurnFinished {
            stop: None,
            turn_id: turn_id.clone(),
            outcome: TurnOutcome::Interrupted,
            stop_reason: "unclean process exit".to_string(),
            rounds: 0,
            modified_files: Vec::new(),
        };
        let (event_type, payload) = event.to_row()?;
        terminal
            .finish_turn_owned(
                token,
                &session_id,
                &turn_id,
                &event_type,
                &payload,
                TurnOutcome::Interrupted,
                leveler_core::now(),
            )
            .await?;
        events.push(event);
    }
    Ok(events)
}

/// Which running turns a reap may settle.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReapScope {
    /// Recovery — at startup, or on opening a session: only turns whose boot
    /// is proven dead. This boot's own turns may be live and are left alone.
    EndedBoots,
    /// This boot shutting down: only the turns it started.
    OwnBoot,
    /// A cancel with nothing of this boot running in the session: this
    /// boot's leftover turns and those of boots proven dead.
    OwnAndEndedBoots,
}

/// Why a reap left a running turn alone.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReapRefusal {
    /// A live boot is running it.
    LiveBoot,
    /// Whether its boot is alive cannot be established: the probe failed, or
    /// the turn predates boot records.
    UnknownOwner,
    /// A different runtime owns the session's task.
    ForeignRuntime,
}

/// A session the reaper refused to touch, and why.
#[derive(Debug)]
pub struct ReapConflict {
    pub session_id: SessionId,
    pub refusal: ReapRefusal,
}

/// A session whose orphan turns were durably reaped under this ownership
/// token. A harness may use the boundary to persist its own recovery facts;
/// the recovery ends when it passes the session to [`release_reaped`], and the
/// token must not be kept past that — recovery starts no execution.
#[derive(Debug, Clone)]
pub struct ReapedSession {
    pub session_id: SessionId,
    pub token: OwnershipToken,
}

/// What a reap did: the reaped events, plus every session whose running turns
/// it explicitly left alone (callers report these; silence would hide a
/// split-ownership situation).
#[derive(Debug, Default)]
pub struct ReapOutcome {
    pub events: Vec<EngineEvent>,
    pub conflicts: Vec<ReapConflict>,
    pub reaped_sessions: Vec<ReapedSession>,
}

/// Settle the running turns `scope` gives this boot authority over. For each
/// session holding one, ownership is acquired through the engine's one
/// acquisition rule — a fresh epoch fences the dead boot's token — and only
/// those turns are interrupted under it. Every other running turn, and every
/// session whose ownership is refused, is left exactly as it was.
pub async fn reap_after_restart(
    engine: &TaskEngine,
    session_id: Option<&SessionId>,
    scope: ReapScope,
) -> Result<ReapOutcome, EngineError> {
    let stores = &engine.stores;
    let running = stores.turns.list_running(session_id).await?;
    let mut outcome = ReapOutcome::default();
    // Running rows come ordered by session, so each session is one run.
    let mut candidates: Vec<(SessionId, Vec<TurnRecord>)> = Vec::new();
    for turn in running {
        let session = SessionId::new(turn.session_id.clone());
        let owner = turn.owner_boot_id.as_deref().map(BootId::new);
        let refusal = match owner {
            None if scope == ReapScope::OwnBoot => continue,
            None => Some(ReapRefusal::UnknownOwner),
            Some(boot) if boot == engine.boot.id => {
                if scope == ReapScope::EndedBoots {
                    continue;
                }
                None
            }
            Some(_) if scope == ReapScope::OwnBoot => continue,
            Some(boot) => match engine.boot.liveness.liveness(&boot) {
                BootLiveness::Dead => None,
                BootLiveness::Alive => Some(ReapRefusal::LiveBoot),
                BootLiveness::Unknown => Some(ReapRefusal::UnknownOwner),
            },
        };
        if let Some(refusal) = refusal {
            outcome.conflicts.push(ReapConflict {
                session_id: session,
                refusal,
            });
            continue;
        }
        match candidates.last_mut() {
            Some((last, turns)) if *last == session => turns.push(turn),
            _ => candidates.push((session, vec![turn])),
        }
    }
    for (session, turns) in candidates {
        let refusal = match engine.acquire_ownership(&session).await {
            Ok(token) => {
                if let Err(error) =
                    reap_session(engine, &session, &token, &turns, &mut outcome).await
                {
                    release_reaped(
                        engine,
                        &[ReapedSession {
                            session_id: session,
                            token,
                        }],
                    )
                    .await;
                    return Err(error);
                }
                continue;
            }
            Err(EngineError::OwnedByLiveBoot { .. }) => ReapRefusal::LiveBoot,
            Err(EngineError::OwnershipUnknown { .. }) => ReapRefusal::UnknownOwner,
            Err(EngineError::OwnershipConflict { .. }) => ReapRefusal::ForeignRuntime,
            Err(error) => return Err(error),
        };
        outcome.conflicts.push(ReapConflict {
            session_id: session,
            refusal,
        });
    }
    Ok(outcome)
}

async fn reap_session(
    engine: &TaskEngine,
    session: &SessionId,
    token: &OwnershipToken,
    turns: &[TurnRecord],
    outcome: &mut ReapOutcome,
) -> Result<(), EngineError> {
    let stores = &engine.stores;
    let events = interrupt_turns_owned(
        stores.messages.as_ref(),
        stores.terminal.as_ref(),
        token,
        turns,
    )
    .await?;
    // A reaped turn's children died with it. Mark them now, under the same
    // fresh token, so nothing reads them as running while the session waits
    // to be resumed.
    let log = crate::EventLog::new_owned(stores.events.as_ref(), session.clone(), token.clone());
    let mut marked = Vec::new();
    log.interrupt_open_children(&mut |event| marked.push(event))
        .await?;
    outcome.events.extend(marked);
    outcome.reaped_sessions.push(ReapedSession {
        session_id: session.clone(),
        token: token.clone(),
    });
    outcome.events.extend(events);
    Ok(())
}

/// End the recovery generations of `reaped`, once the harness has written
/// whatever recovery facts it keeps under them. Recovery is finite work: the
/// recovering boot keeps running, and the next execution — from any boot —
/// acquires anew. A release that fails leaves the session to this boot until
/// it exits; that is reported, not hidden.
pub async fn release_reaped(engine: &TaskEngine, reaped: &[ReapedSession]) {
    for reaped in reaped {
        if let Err(error) = engine.release_ownership(&reaped.token).await {
            tracing::warn!(
                %error,
                session = %reaped.session_id,
                "recovery could not release the session it settled"
            );
        }
    }
}
