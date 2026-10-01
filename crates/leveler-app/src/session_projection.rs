//! One status projection for live snapshots, restored sessions and the task directory.
use leveler_client_protocol::{ClientError, UiTaskStatus, UiTaskTerminal};
use leveler_core::{BootId, BootLiveness, BootLivenessProbe};
use leveler_storage::SessionFacts;

/// Project a title using the same first-message policy as interactive sessions.
pub fn project_title(goal: &str, first_user_text: Option<&str>) -> String {
    if goal != "interactive session" && !goal.trim().is_empty() {
        return goal.to_owned();
    }
    first_user_text
        .and_then(title_from_first_message)
        .unwrap_or_else(|| goal.to_owned())
}

/// Derive the bounded title from the first sentence of the first nonempty line.
pub fn title_from_first_message(content: &str) -> Option<String> {
    let line = content
        .trim()
        .lines()
        .find(|line| !line.trim().is_empty())?
        .trim();
    let sentence = line
        .split(['。', '？', '！', '?', '!', '；', ';'])
        .next()
        .unwrap_or(line)
        .trim();
    let title: String = sentence.chars().take(40).collect();
    (!title.is_empty()).then_some(title)
}

/// Project only durable task evidence or the current runtime's actual waiter.
/// A boot's existence cannot prove a pending approval; only `waiting` may do so.
pub fn project_task(
    facts: &SessionFacts,
    liveness: &dyn BootLivenessProbe,
    live: bool,
    waiting: bool,
) -> Result<(UiTaskStatus, Option<UiTaskTerminal>), ClientError> {
    if let Some(row) = &facts.terminal {
        let newer_start = facts
            .latest_start_sequence
            .is_some_and(|sequence| sequence > row.sequence);
        // A newly admitted turn can exist before its first event is appended.
        let newer_turn = facts.turns.last().is_some_and(|turn| {
            facts
                .terminal_turn_ordinal
                .is_none_or(|ordinal| turn.ordinal > ordinal)
        });
        if !newer_start && !newer_turn {
            if row.schema_version > leveler_storage::EVENT_SCHEMA_VERSION {
                return Err(ClientError::Runtime(
                    "task terminal schema is newer than this reader".into(),
                ));
            }
            let event = leveler_engine::EngineEvent::from_payload(&row.payload)
                .map_err(|error| ClientError::Runtime(format!("task terminal: {error}")))?;
            let leveler_engine::EngineEvent::TaskFinished {
                outcome,
                stop,
                reason,
                warnings,
                ..
            } = event
            else {
                return Err(ClientError::Runtime(
                    "task_finished row carried a different event".into(),
                ));
            };
            use leveler_lifecycle::{StopReason, TaskOutcome};
            let status = match outcome {
                TaskOutcome::Cancelled => UiTaskStatus::Cancelled,
                TaskOutcome::Interrupted => UiTaskStatus::Interrupted,
                TaskOutcome::Failed => UiTaskStatus::Failed,
                TaskOutcome::Blocked => UiTaskStatus::Blocked,
                TaskOutcome::BudgetLimited => UiTaskStatus::Incomplete,
                TaskOutcome::Completed => match stop {
                    Some(StopReason::Completed) => UiTaskStatus::Completed,
                    Some(StopReason::Answered) => UiTaskStatus::Answered,
                    Some(StopReason::Blocked) => UiTaskStatus::Blocked,
                    Some(
                        StopReason::Incomplete
                        | StopReason::BudgetExhausted
                        | StopReason::TurnLimitReached
                        | StopReason::Stalled,
                    ) => UiTaskStatus::Incomplete,
                    // Legacy TaskFinished remains canonical terminal evidence;
                    // absent stop remains absent rather than inventing a declaration.
                    None => UiTaskStatus::Completed,
                },
            };
            let terminal = UiTaskTerminal {
                sequence: row.sequence,
                outcome: outcome.as_str().into(),
                stop: stop.map(|reason| {
                    match reason {
                        StopReason::Completed => "completed",
                        StopReason::Answered => "answered",
                        StopReason::Incomplete => "incomplete",
                        StopReason::BudgetExhausted => "budget_exhausted",
                        StopReason::TurnLimitReached => "turn_limit_reached",
                        StopReason::Blocked => "blocked",
                        StopReason::Stalled => "stalled",
                    }
                    .into()
                }),
                reason,
                warnings,
            };
            return Ok((status, Some(terminal)));
        }
    }
    if live {
        return Ok((
            if waiting {
                UiTaskStatus::WaitingUser
            } else {
                UiTaskStatus::Running
            },
            None,
        ));
    }
    let running: Vec<_> = facts
        .turns
        .iter()
        .filter(|turn| turn.status == "running")
        .collect();
    if !running.is_empty() {
        let mut unknown = false;
        for turn in running {
            match turn
                .owner_boot_id
                .as_deref()
                .map(BootId::new)
                .map(|id| liveness.liveness(&id))
            {
                Some(BootLiveness::Alive) => return Ok((UiTaskStatus::Running, None)),
                Some(BootLiveness::Dead) => {}
                _ => unknown = true,
            }
        }
        return Ok((
            if unknown {
                UiTaskStatus::Unknown
            } else {
                UiTaskStatus::Interrupted
            },
            None,
        ));
    }
    if facts.session.status == leveler_lifecycle::SessionStatus::Running {
        return Ok((UiTaskStatus::Interrupted, None));
    }
    if facts.terminal.is_some() {
        return Ok((UiTaskStatus::Unknown, None));
    }
    if facts.turns.is_empty() && facts.session.status == leveler_lifecycle::SessionStatus::Created {
        return Ok((UiTaskStatus::Idle, None));
    }
    Ok((UiTaskStatus::Unknown, None))
}

#[cfg(test)]
mod tests {
    use super::*;
    use leveler_lifecycle::{StopReason, TaskOutcome};
    struct Probe(BootLiveness);
    impl BootLivenessProbe for Probe {
        fn liveness(&self, _: &BootId) -> BootLiveness {
            self.0
        }
    }
    fn facts() -> SessionFacts {
        SessionFacts {
            session: leveler_storage::SessionRecord::without_workspace(
                "task",
                "m",
                leveler_core::now(),
            ),
            archived_at: None,
            last_activity_at: "t".into(),
            first_user_text: None,
            turns: Vec::new(),
            terminal: None,
            terminal_turn_ordinal: None,
            latest_start_sequence: None,
        }
    }
    fn finish(facts: &mut SessionFacts, stop: StopReason) {
        let (tag, payload) = leveler_engine::EngineEvent::TaskFinished {
            outcome: TaskOutcome::Completed,
            reason: None,
            stop: Some(stop),
            failure: None,
            warnings: vec![],
        }
        .to_row()
        .unwrap();
        facts.terminal = Some(leveler_storage::EventRecord {
            id: "event".into(),
            session_id: facts.session.id.clone(),
            turn_id: None,
            sequence: 2,
            event_type: tag,
            payload,
            created_at: "t".into(),
            schema_version: 1,
        });
        facts.latest_start_sequence = Some(1);
    }
    #[test]
    fn legacy_committed_terminal_retains_outcome_without_inventing_declaration() {
        let mut facts = facts();
        finish(&mut facts, StopReason::Completed);
        let event = leveler_engine::EngineEvent::TaskFinished {
            outcome: TaskOutcome::Completed,
            stop: None,
            reason: None,
            failure: None,
            warnings: Vec::new(),
        };
        facts.terminal.as_mut().unwrap().payload = event.to_row().unwrap().1;
        let (status, terminal) =
            project_task(&facts, &Probe(BootLiveness::Unknown), false, false).unwrap();
        assert_eq!(status, UiTaskStatus::Completed);
        assert_eq!(terminal.unwrap().stop, None);
    }
    #[test]
    fn canonical_terminal_outlives_postcommit_turn_lease() {
        let mut facts = facts();
        facts.turns.push(leveler_storage::TurnRecord {
            id: "settled".into(),
            session_id: facts.session.id.clone(),
            ordinal: 1,
            kind: "chat".into(),
            payload: None,
            status: "completed".into(),
            created_at: "t".into(),
            finished_at: Some("t".into()),
            owner_boot_id: Some("boot".into()),
        });
        facts.terminal_turn_ordinal = Some(1);
        finish(&mut facts, StopReason::Answered);
        assert_eq!(
            project_task(&facts, &Probe(BootLiveness::Alive), true, true)
                .unwrap()
                .0,
            UiTaskStatus::Answered
        );
        finish(&mut facts, StopReason::Completed);
        assert_eq!(
            project_task(&facts, &Probe(BootLiveness::Alive), true, false)
                .unwrap()
                .0,
            UiTaskStatus::Completed
        );
    }
    #[test]
    fn answered_is_not_completed_and_historical_terminal_cannot_finish_new_turn() {
        let mut facts = facts();
        finish(&mut facts, StopReason::Answered);
        assert_eq!(
            project_task(&facts, &Probe(BootLiveness::Unknown), false, false)
                .unwrap()
                .0,
            UiTaskStatus::Answered
        );
        finish(&mut facts, StopReason::Completed);
        assert_eq!(
            project_task(&facts, &Probe(BootLiveness::Unknown), false, false)
                .unwrap()
                .0,
            UiTaskStatus::Completed
        );
        facts.latest_start_sequence = Some(3);
        assert_eq!(
            project_task(&facts, &Probe(BootLiveness::Unknown), false, false).unwrap(),
            (UiTaskStatus::Unknown, None)
        );
    }
    #[test]
    fn running_requires_live_runtime_or_live_boot_and_waiter_is_not_rebuilt() {
        let mut facts = facts();
        finish(&mut facts, StopReason::Completed);
        // Admission precedes the first TurnStarted event. A durable row alone
        // must prevent the preceding terminal from completing the new work.
        facts.turns.push(leveler_storage::TurnRecord {
            id: "turn".into(),
            session_id: facts.session.id.clone(),
            ordinal: 1,
            kind: "chat".into(),
            payload: None,
            status: "running".into(),
            created_at: "t".into(),
            finished_at: None,
            owner_boot_id: Some("boot".into()),
        });
        for (boot, status) in [
            (BootLiveness::Alive, UiTaskStatus::Running),
            (BootLiveness::Dead, UiTaskStatus::Interrupted),
            (BootLiveness::Unknown, UiTaskStatus::Unknown),
        ] {
            assert_eq!(
                project_task(&facts, &Probe(boot), false, true).unwrap().0,
                status
            );
        }
        assert_eq!(
            project_task(&facts, &Probe(BootLiveness::Unknown), true, true)
                .unwrap()
                .0,
            UiTaskStatus::WaitingUser
        );
    }
    #[test]
    fn text_or_legacy_completed_status_is_not_terminal_evidence() {
        let mut facts = facts();
        facts.session.status = leveler_lifecycle::SessionStatus::Completed;
        facts.first_user_text = Some("completed".into());
        assert_eq!(
            project_task(&facts, &Probe(BootLiveness::Unknown), false, false).unwrap(),
            (UiTaskStatus::Unknown, None)
        );
        finish(&mut facts, StopReason::Completed);
        facts.terminal.as_mut().unwrap().payload = "invalid".into();
        assert!(project_task(&facts, &Probe(BootLiveness::Unknown), false, false).is_err());
    }
}
