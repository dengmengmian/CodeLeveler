//! Continuing a delegated child across a runtime window.
//!
//! The engine records that a child's activation died and asks this harness
//! whether it continues the child. The answer needs Coding knowledge — which
//! roles are worth continuing, whether the parent's epoch still wants the
//! work, whether the child had already handed its findings over — so it lives
//! here, and so does rebuilding the child's context from its durable session.

use leveler_core::SessionId;
use leveler_engine::{EngineError, EngineEvent, LostChild, MAX_CHILD_RESUMES, ResumedChild};
use leveler_lifecycle::ChildSpawnSpec;
use leveler_model::{Message, Role};
use leveler_storage::EventStore;

use crate::child_profile::AgentRole;
use crate::sub_agent::ResumableChild;

/// The interrupted children this harness continues: every one that recorded
/// a spec and a transcript, is not a reviewer, has handed nothing over yet, in
/// an epoch the parent still has open.
pub(crate) async fn continuable_children(
    events: &dyn EventStore,
    session_id: &SessionId,
    interrupted: &[LostChild],
) -> Result<Vec<String>, EngineError> {
    let plan = crate::coding::turn::last_persisted_plan(events, session_id).await?;
    let progress = crate::coding::turn::last_persisted_progress(events, session_id).await?;
    if !crate::coding::run::prior_epoch_open(plan.as_ref(), progress.as_ref()) {
        return Ok(Vec::new());
    }
    let adopted = crate::coding::turn::last_persisted_ledger(events, session_id)
        .await?
        .map(|ledger| ledger.findings)
        .unwrap_or_default();
    let rows = events
        .load_by_types(
            session_id,
            &["sub_agent_started", "sub_agent_transcript_appended"],
        )
        .await?;
    let mut with_spec: Vec<String> = Vec::new();
    let mut with_transcript: Vec<String> = Vec::new();
    for row in &rows {
        match EngineEvent::from_payload(&row.payload)? {
            EngineEvent::SubAgentStarted {
                id, spec: Some(_), ..
            } => with_spec.push(id),
            EngineEvent::SubAgentTranscriptAppended { id, messages } if !messages.is_empty() => {
                with_transcript.push(id)
            }
            _ => {}
        }
    }
    Ok(interrupted
        .iter()
        .filter(|child| {
            matches!(
                AgentRole::from_label(&child.role),
                Some(AgentRole::Default | AgentRole::Explorer | AgentRole::Worker)
            ) && with_spec.contains(&child.id)
                && with_transcript.contains(&child.id)
                && !adopted.iter().any(|f| f.source_child == child.id)
        })
        .map(|child| child.id.clone())
        .collect())
}

/// Rebuild each resumed child from its durable session: spec, transcript, and
/// the tool facts recorded after its last saved round.
pub(crate) async fn load_resumable_children(
    events: &dyn EventStore,
    session_id: &SessionId,
    resumed: &[ResumedChild],
) -> Result<Vec<ResumableChild>, EngineError> {
    if resumed.is_empty() {
        return Ok(Vec::new());
    }
    let rows = events
        .load_by_types(
            session_id,
            &[
                "sub_agent_started",
                "sub_agent_transcript_appended",
                "tool_call_started",
                "tool_call_finished",
            ],
        )
        .await?;
    let decoded = rows
        .iter()
        .map(|row| EngineEvent::from_payload(&row.payload).map(|event| (row.sequence, event)))
        .collect::<Result<Vec<_>, _>>()?;

    let mut out = Vec::with_capacity(resumed.len());
    for child in resumed {
        let role = AgentRole::from_label(&child.role).ok_or_else(|| {
            EngineError::Corrupt(format!(
                "resumed child {} has an unknown role `{}`",
                child.id, child.role
            ))
        })?;
        let spec = decoded
            .iter()
            .find_map(|(_, event)| match event {
                EngineEvent::SubAgentStarted { id, spec, .. } if *id == child.id => spec.clone(),
                _ => None,
            })
            .ok_or_else(|| {
                EngineError::Corrupt(format!("resumed child {} recorded no spec", child.id))
            })?;
        let mut prior = Vec::new();
        let mut watermark = None;
        for (sequence, event) in &decoded {
            if let EngineEvent::SubAgentTranscriptAppended { id, messages } = event
                && *id == child.id
            {
                prior.extend(messages.iter().cloned());
                watermark = Some(*sequence);
            }
        }
        let after_last_round = unsettled_tool_calls(&decoded, &child.id, watermark);
        let note = recovery_note(child.attempt, role, &spec, &after_last_round);
        out.push(ResumableChild {
            id: child.id.clone(),
            nickname: child.nickname.clone(),
            role,
            spec,
            prior,
            note: Message::text(Role::User, note),
        });
    }
    Ok(out)
}

/// A tool call the child made after its last saved round: its result is not
/// in the restored transcript, so the child must be told it exists.
struct CallAfterRound {
    name: String,
    arguments: String,
    /// `Some(is_error)` when a terminal was recorded; `None` when the outcome
    /// is unknown.
    finished: Option<bool>,
}

fn unsettled_tool_calls(
    decoded: &[(i64, EngineEvent)],
    child: &str,
    watermark: Option<i64>,
) -> Vec<CallAfterRound> {
    let after = |sequence: i64| watermark.is_none_or(|w| sequence > w);
    let mut calls: Vec<(String, CallAfterRound)> = Vec::new();
    for (sequence, event) in decoded {
        if !after(*sequence) {
            continue;
        }
        match event {
            EngineEvent::ToolCallStarted {
                call_id,
                name,
                arguments,
                agent_id: Some(agent),
                ..
            } if agent == child => calls.push((
                call_id.clone(),
                CallAfterRound {
                    name: name.clone(),
                    arguments: leveler_core::truncate_head_bytes(arguments, 200, "…"),
                    finished: None,
                },
            )),
            EngineEvent::ToolCallFinished {
                call_id,
                is_error,
                agent_id: Some(agent),
                ..
            } if agent == child => {
                if let Some((_, call)) = calls.iter_mut().find(|(id, _)| id == call_id) {
                    call.finished = Some(*is_error);
                }
            }
            _ => {}
        }
    }
    calls.into_iter().map(|(_, call)| call).collect()
}

/// The host-authored note a resumed child reads before anything else. States
/// facts — the interruption, what authority it holds now, which effects may
/// already have happened — and nothing about how to do its task.
fn recovery_note(
    attempt: u32,
    role: AgentRole,
    spec: &ChildSpawnSpec,
    calls: &[CallAfterRound],
) -> String {
    let mut out = format!(
        "## Resumed after an interruption\n\
         Your previous activation ended when its runtime window died (resume \
         {attempt} of at most {MAX_CHILD_RESUMES}). You are the same sub-agent \
         continuing the same task; the conversation above is your own earlier \
         work.\n"
    );
    match role {
        AgentRole::Worker if !spec.files.is_empty() => out.push_str(&format!(
            "Your exclusive write scope ({}) is re-claimed for this activation.\n",
            spec.files.join(", ")
        )),
        AgentRole::Default => out.push_str(
            "You hold no write scope now: any scope claimed before the interruption \
             was released. Call claim_write_scope again before editing.\n",
        ),
        _ => {}
    }
    if calls.is_empty() {
        out.push_str("No tool call was recorded after your last saved round.\n");
    } else {
        out.push_str(
            "Tool calls recorded after your last saved round (their results are not above):\n",
        );
        for call in calls {
            let outcome = match call.finished {
                Some(false) => "finished",
                Some(true) => "finished with an error",
                None => "outcome unknown: it may already have taken effect",
            };
            out.push_str(&format!("- {} {} — {outcome}\n", call.name, call.arguments));
        }
    }
    out.push_str(
        "Continue the task from where it stands. Inspect before repeating anything that \
         may already have happened.",
    );
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_note_names_an_unknown_outcome_rather_than_assuming_one() {
        let note = recovery_note(
            1,
            AgentRole::Worker,
            &ChildSpawnSpec {
                files: vec!["src/a.rs".into()],
                ..Default::default()
            },
            &[CallAfterRound {
                name: "apply_patch".into(),
                arguments: "{…}".into(),
                finished: None,
            }],
        );
        assert!(note.contains("re-claimed"), "{note}");
        assert!(note.contains("outcome unknown"), "{note}");
    }
}
