//! The ONE canonical GoalCheckpoint builder (long-goal P3).
//!
//! Every trigger — `/recap`, milestone, interruption, context compaction —
//! projects through this module, so a Recap in the TUI, the compaction
//! breadcrumb, and the resume context all present the SAME persisted facts.
//! Nothing here asks the model what the runtime already knows: structured
//! facts come from the event log, the evidence ledger, and bounded git
//! metadata. The optional semantic wording is applied by the caller on top
//! and can fail without costing the structured checkpoint.
//!
//! Cursor discipline: [`project_goal_checkpoint`] reads the committed
//! `MAX(sequence)` of the session's event log. The CALLER owns making that
//! read safe — every trigger sits behind a durable boundary (the event flush
//! barrier, a committed terminal turn, or the reaper's fenced commit), so
//! the cursor can never point beyond durable EventLog state.

use std::path::Path;

use leveler_core::SessionId;
use leveler_lifecycle::{
    CheckpointChild, CheckpointFindings, CheckpointPlan, CheckpointReason, CheckpointVerification,
    CheckpointWorkspace, EvidenceLedger, FindingState, GoalCheckpoint,
};
use leveler_storage::{
    EngineStores, EventStore, GoalCheckpointRecord, GoalRecord, GoalState, MessageStore,
    NewGoalCheckpoint,
};

use crate::EngineError;
use crate::event::EngineEvent;

/// How many settled children / finding refs / changed paths a checkpoint
/// carries at most. Counts stay authoritative when a list is truncated.
const MAX_REFS: usize = 20;

/// The deterministic projection: payload plus the boundary it represents.
#[derive(Debug, Clone)]
pub struct ProjectedCheckpoint {
    pub payload: GoalCheckpoint,
    /// Inclusive committed event boundary of the goal's session.
    /// `0` = no events yet (the delta is the whole log).
    pub event_cursor: i64,
}

/// Project the structured checkpoint facts for `goal` out of authoritative
/// state. Pure reads; nothing is persisted and no model is called.
pub async fn project_goal_checkpoint(
    events: &dyn EventStore,
    messages: &dyn MessageStore,
    goal: &GoalRecord,
    session_id: &SessionId,
    repo: Option<&Path>,
) -> Result<ProjectedCheckpoint, EngineError> {
    let event_cursor = events.latest_sequence(session_id).await?.unwrap_or(0);
    let transcript_ordinal = messages.load(session_id).await?.len() as u64;

    let ledger = last_ledger(events, session_id).await?;
    let (findings, verification) = match &ledger {
        Some(ledger) => (findings_from(ledger), verification_from(ledger)),
        // The ledger could not be read / was never written: explicitly
        // unknown and unmeasured — never zero, never passed.
        None => (
            CheckpointFindings::Unknown,
            CheckpointVerification::Unmeasured,
        ),
    };
    let plan = match &ledger {
        Some(ledger) => CheckpointPlan::from_state(&ledger.plan),
        None => last_plan(events, session_id)
            .await?
            .as_ref()
            .and_then(CheckpointPlan::from_state),
    };

    let payload = GoalCheckpoint {
        objective: goal.objective.clone(),
        transcript_ordinal: Some(transcript_ordinal),
        plan,
        verification,
        findings,
        children: settled_children(events, session_id).await?,
        artifact_refs: Vec::new(),
        workspace: match repo {
            Some(repo) => capture_workspace(repo).await,
            None => CheckpointWorkspace::default(),
        },
        ..Default::default()
    }
    .bounded();

    Ok(ProjectedCheckpoint {
        payload,
        event_cursor,
    })
}

/// The optional semantic wording a trigger may add on top of the structured
/// facts. Every field is optional and bounded later; absence degrades the
/// wording, never the checkpoint.
#[derive(Debug, Clone, Default)]
pub struct SemanticRecap {
    /// Concise prose summary of the work so far (feeds the context block).
    pub goal_summary: Option<String>,
    /// The 1–2 line presentation; deterministic fallback used when absent.
    pub display_summary: Option<String>,
    /// Next-action wording; the plan's next step stands in when absent.
    pub next_action: Option<String>,
}

impl SemanticRecap {
    /// Wrap a compaction-style briefing paragraph: it becomes the goal
    /// summary only — display stays deterministic.
    pub fn briefing(summary: Option<&str>) -> Option<Self> {
        summary.map(|s| Self {
            goal_summary: Some(s.to_string()),
            ..Default::default()
        })
    }
}

/// Resolve the session's goal, project, and PERSIST a checkpoint. The one
/// creation seam every trigger calls.
///
/// `Ok(None)` = the session has no goal to checkpoint (truthful absence, not
/// an error): triggers proceed without one, `/recap` reports it to the user.
/// A running goal is preferred; a settled one is accepted only for `Manual`
/// (a user may ask for a recap after the run ended). `semantic_summary`, when
/// present, becomes the payload's goal summary — structured facts never
/// depend on it.
///
/// The caller owns the durable barrier BEFORE this call (event flush /
/// committed terminal / reaper commit), so the captured cursor is
/// committed-only by construction.
pub async fn create_goal_checkpoint(
    stores: &EngineStores,
    session_id: &SessionId,
    reason: CheckpointReason,
    repo: Option<&Path>,
    semantic: Option<SemanticRecap>,
) -> Result<Option<GoalCheckpointRecord>, EngineError> {
    let Some(task) = stores.tasks.task_for_session(session_id).await? else {
        return Ok(None);
    };
    let goals = stores.goals.for_task(&task).await?;
    let goal = goals
        .iter()
        .find(|g| g.state == GoalState::Running)
        .or_else(|| {
            matches!(reason, CheckpointReason::Manual)
                .then(|| goals.first())
                .flatten()
        });
    let Some(goal) = goal else {
        return Ok(None);
    };
    let projected = project_goal_checkpoint(
        stores.events.as_ref(),
        stores.messages.as_ref(),
        goal,
        session_id,
        repo,
    )
    .await?;
    let mut payload = projected.payload;
    if let Some(semantic) = semantic {
        payload.goal_summary = semantic.goal_summary;
        payload.display_summary = semantic.display_summary;
        payload.next_action = semantic.next_action;
    }
    let record = stores
        .goal_checkpoints
        .create(
            NewGoalCheckpoint {
                goal_id: goal.id.clone(),
                session_id: session_id.clone(),
                reason,
                event_cursor: projected.event_cursor,
                payload: payload.bounded(),
            },
            leveler_core::now(),
        )
        .await?;
    Ok(Some(record))
}

/// The canonical event announcing a persisted checkpoint. Emitted AFTER the
/// row exists, so replay never names a checkpoint that was not stored.
pub fn checkpoint_created_event(record: &GoalCheckpointRecord) -> EngineEvent {
    EngineEvent::GoalCheckpointCreated {
        checkpoint_id: record.id.as_str().to_string(),
        goal_id: record.goal_id.as_str().to_string(),
        reason: record.reason.as_str().to_string(),
        created_at: record.created_at.to_rfc3339(),
        payload: Box::new(record.payload.clone()),
    }
}

/// Continuation context from the latest valid checkpoint: the rendered
/// `[GOAL CHECKPOINT]` block plus EXACTLY the transcript messages after its
/// watermark — never a replay of what the checkpoint already represents.
///
/// `Ok(None)` = no usable checkpoint; the caller keeps the pre-checkpoint
/// full-history path (backward compatibility, and the fail-closed answer to
/// a corrupt/stale/future checkpoint — trust nothing, fall back).
pub async fn resume_prior_from_checkpoint(
    stores: &EngineStores,
    session_id: &SessionId,
    transcript: &[leveler_model::Message],
) -> Result<Option<Vec<leveler_model::Message>>, EngineError> {
    let Some(task) = stores.tasks.task_for_session(session_id).await? else {
        return Ok(None);
    };
    let goals = stores.goals.for_task(&task).await?;
    // Resume continues the goal still owing work when there is one;
    // otherwise the most recent goal is the one whose history this is.
    let goal = goals
        .iter()
        .find(|g| g.state == GoalState::Running)
        .or_else(|| goals.first());
    let Some(goal) = goal else {
        return Ok(None);
    };
    let Some(checkpoint) = stores.goal_checkpoints.latest_for_goal(&goal.id).await? else {
        return Ok(None);
    };
    // Cursor sanity: a checkpoint may only reference committed events. A
    // cursor beyond the durable log is corruption — never trusted.
    let latest = stores
        .events
        .latest_sequence(session_id)
        .await?
        .unwrap_or(0);
    if checkpoint.event_cursor > latest {
        tracing::warn!(
            checkpoint = %checkpoint.id,
            cursor = checkpoint.event_cursor,
            latest,
            "checkpoint cursor is beyond the durable event log; falling back to full history"
        );
        return Ok(None);
    }
    let Some(ordinal) = checkpoint.payload.transcript_ordinal else {
        return Ok(None);
    };
    let ordinal = ordinal as usize;
    if ordinal > transcript.len() {
        tracing::warn!(
            checkpoint = %checkpoint.id,
            ordinal,
            transcript = transcript.len(),
            "checkpoint transcript watermark is beyond the transcript; falling back"
        );
        return Ok(None);
    }
    let mut prior = Vec::with_capacity(1 + transcript.len() - ordinal);
    prior.push(leveler_model::Message {
        role: leveler_model::Role::User,
        content: vec![leveler_model::ContentPart::Text {
            text: checkpoint.payload.context_block(),
        }],
    });
    prior.extend_from_slice(&transcript[ordinal..]);
    Ok(Some(prior))
}

async fn last_ledger(
    events: &dyn EventStore,
    session_id: &SessionId,
) -> Result<Option<EvidenceLedger>, EngineError> {
    let Some(row) = events
        .load_last_by_type(session_id, "evidence_ledger_updated", None)
        .await?
    else {
        return Ok(None);
    };
    match EngineEvent::from_payload(&row.payload)? {
        EngineEvent::EvidenceLedgerUpdated { ledger } => Ok(Some(ledger)),
        _ => Err(EngineError::Corrupt(
            "evidence_ledger_updated row carried a different event".into(),
        )),
    }
}

async fn last_plan(
    events: &dyn EventStore,
    session_id: &SessionId,
) -> Result<Option<leveler_lifecycle::PlanState>, EngineError> {
    let Some(row) = events
        .load_last_by_type(session_id, "plan_updated", None)
        .await?
    else {
        return Ok(None);
    };
    match EngineEvent::from_payload(&row.payload)? {
        EngineEvent::PlanUpdated { steps } => Ok(Some(leveler_lifecycle::PlanState { steps })),
        _ => Err(EngineError::Corrupt(
            "plan_updated row carried a different event".into(),
        )),
    }
}

fn findings_from(ledger: &EvidenceLedger) -> CheckpointFindings {
    let open: Vec<&leveler_lifecycle::FindingRecord> = ledger
        .findings
        .iter()
        .filter(|f| !matches!(f.state, FindingState::Rejected | FindingState::Verified))
        .collect();
    CheckpointFindings::Known {
        total: ledger.findings.len() as u32,
        open: open.len() as u32,
        open_blocking: ledger.findings.iter().filter(|f| f.open_blocking()).count() as u32,
        refs: open.iter().take(MAX_REFS).map(|f| f.id.clone()).collect(),
    }
}

fn verification_from(ledger: &EvidenceLedger) -> CheckpointVerification {
    if ledger.has_fresh_successful_verify() {
        let evidence = ledger
            .verifications
            .iter()
            .rev()
            .find(|v| v.exit_code == 0)
            .map(|v| v.command_fingerprint.clone())
            .unwrap_or_else(|| "fresh successful verification".to_string());
        return CheckpointVerification::Passed { evidence };
    }
    match ledger.verifications.last() {
        Some(last) if last.exit_code != 0 => CheckpointVerification::Failed {
            detail: format!("{} (exit {})", last.command_fingerprint, last.exit_code),
        },
        // A stale or baseline-green pass proves nothing about the current
        // state; "not measured" is the truthful reading, not "passed".
        _ => CheckpointVerification::Unmeasured,
    }
}

async fn settled_children(
    events: &dyn EventStore,
    session_id: &SessionId,
) -> Result<Vec<CheckpointChild>, EngineError> {
    let rows = events
        .load_by_types(session_id, &["sub_agent_finished"])
        .await?;
    let mut out = Vec::new();
    for row in rows {
        if let EngineEvent::SubAgentFinished {
            id,
            nickname,
            ok,
            contribution,
            ..
        } = EngineEvent::from_payload(&row.payload)?
        {
            out.push(CheckpointChild {
                child_id: id,
                nickname,
                completed: ok,
                contribution,
            });
        }
    }
    // Keep the most recent settlements when a long session had many.
    if out.len() > MAX_REFS {
        out.drain(..out.len() - MAX_REFS);
    }
    Ok(out)
}

/// Bounded git metadata. Every failure yields `None` — unknown, never an
/// assumed-clean workspace. Never captures diffs or file contents.
async fn capture_workspace(repo: &Path) -> CheckpointWorkspace {
    let head = git_line(repo, &["rev-parse", "HEAD"]).await;
    let branch = git_line(repo, &["rev-parse", "--abbrev-ref", "HEAD"]).await;
    let status = git_output(repo, &["status", "--porcelain"]).await;
    let (dirty, changed_paths) = match status {
        Some(text) => {
            let paths: Vec<String> = text
                .lines()
                .filter(|l| l.len() > 3)
                .take(MAX_REFS)
                .map(|l| l[3..].trim().to_string())
                .collect();
            (Some(!text.trim().is_empty()), paths)
        }
        None => (None, Vec::new()),
    };
    CheckpointWorkspace {
        branch,
        head,
        dirty,
        changed_paths,
    }
}

async fn git_line(repo: &Path, args: &[&str]) -> Option<String> {
    let text = git_output(repo, args).await?;
    let line = text.trim().to_string();
    (!line.is_empty()).then_some(line)
}

async fn git_output(repo: &Path, args: &[&str]) -> Option<String> {
    let repo = repo.to_path_buf();
    let args: Vec<String> = args.iter().map(|s| s.to_string()).collect();
    tokio::task::spawn_blocking(move || {
        let args: Vec<&str> = args.iter().map(String::as_str).collect();
        leveler_core::git_stdout(&repo, &args)
    })
    .await
    .ok()?
}

#[cfg(test)]
mod tests {
    use super::*;
    use leveler_core::{GoalId, TaskId};
    use leveler_lifecycle::{FindingKind, FindingRecord};
    use leveler_storage::{MemoryEventStore, MemoryMessageStore};

    fn goal() -> GoalRecord {
        GoalRecord {
            id: GoalId::new("g1"),
            task_id: TaskId::new("t1"),
            objective: "port the parser".to_string(),
            state: leveler_storage::GoalState::Running,
            opened_at: leveler_core::now(),
            settled_at: None,
            windows_run: 0,
        }
    }

    async fn append(events: &MemoryEventStore, session: &SessionId, event: EngineEvent) {
        let (event_type, payload) = event.to_row().unwrap();
        events
            .append(session, None, &event_type, &payload, leveler_core::now())
            .await
            .unwrap();
    }

    fn finding(id: &str, state: FindingState, blocking: bool) -> FindingRecord {
        FindingRecord {
            id: id.to_string(),
            source_child: "c1".to_string(),
            role: "explorer".to_string(),
            kind: FindingKind::Correctness,
            summary: format!("finding {id}"),
            file: None,
            symbol: None,
            blocking,
            state,
            resolution_reason: None,
        }
    }

    /// Truth case B/C: with no ledger written, findings are UNKNOWN (not
    /// zero) and verification is UNMEASURED (not passed).
    #[tokio::test]
    async fn no_ledger_projects_unknown_not_success() {
        let events = MemoryEventStore::new();
        let messages = MemoryMessageStore::new();
        let session = SessionId::new("s1");
        let projected = project_goal_checkpoint(&events, &messages, &goal(), &session, None)
            .await
            .unwrap();
        assert_eq!(projected.payload.findings, CheckpointFindings::Unknown);
        assert_eq!(
            projected.payload.verification,
            CheckpointVerification::Unmeasured
        );
        assert_eq!(projected.event_cursor, 0, "no events → empty boundary");
        assert_eq!(projected.payload.transcript_ordinal, Some(0));
        assert_eq!(projected.payload.workspace.dirty, None, "no repo → unknown");
    }

    /// The cursor is the committed MAX(sequence) — never invented, never
    /// beyond what the store holds.
    #[tokio::test]
    async fn cursor_is_the_committed_boundary() {
        let events = MemoryEventStore::new();
        let messages = MemoryMessageStore::new();
        let session = SessionId::new("s1");
        for _ in 0..3 {
            append(
                &events,
                &session,
                EngineEvent::GoalIntercepted {
                    kind: "k".into(),
                    detail: "d".into(),
                },
            )
            .await;
        }
        let projected = project_goal_checkpoint(&events, &messages, &goal(), &session, None)
            .await
            .unwrap();
        assert_eq!(projected.event_cursor, 3);
    }

    /// Truth case D: an open blocking finding stays open and blocking in the
    /// projection; rejected/verified findings are not "open".
    #[tokio::test]
    async fn findings_truth_is_preserved() {
        let events = MemoryEventStore::new();
        let messages = MemoryMessageStore::new();
        let session = SessionId::new("s1");
        let ledger = EvidenceLedger {
            findings: vec![
                finding("f-1", FindingState::Acknowledged, true),
                finding("f-2", FindingState::Rejected, true),
                finding("f-3", FindingState::Verified, false),
            ],
            ..Default::default()
        };
        append(
            &events,
            &session,
            EngineEvent::EvidenceLedgerUpdated { ledger },
        )
        .await;

        let projected = project_goal_checkpoint(&events, &messages, &goal(), &session, None)
            .await
            .unwrap();
        match projected.payload.findings {
            CheckpointFindings::Known {
                total,
                open,
                open_blocking,
                refs,
            } => {
                assert_eq!(total, 3);
                assert_eq!(open, 1, "rejected/verified are settled");
                assert_eq!(open_blocking, 1);
                assert_eq!(refs, vec!["f-1".to_string()]);
            }
            other => panic!("expected known findings, got {other:?}"),
        }
    }

    /// Truth case A vs the stale-pass trap: a fresh successful verify is
    /// PASSED; a verify that predates the last mutation is NOT.
    #[tokio::test]
    async fn verification_truth_requires_fresh_evidence() {
        let events = MemoryEventStore::new();
        let messages = MemoryMessageStore::new();
        let session = SessionId::new("s1");

        let mut fresh = EvidenceLedger::default();
        fresh.record_mutation("m1", "apply_patch", vec!["src/a.rs".into()]);
        fresh.record_verify("v1", "cargo test", 0);
        append(
            &events,
            &session,
            EngineEvent::EvidenceLedgerUpdated { ledger: fresh },
        )
        .await;
        let projected = project_goal_checkpoint(&events, &messages, &goal(), &session, None)
            .await
            .unwrap();
        assert!(matches!(
            projected.payload.verification,
            CheckpointVerification::Passed { .. }
        ));

        // Now a later mutation invalidates that pass.
        let mut stale = EvidenceLedger::default();
        stale.record_mutation("m1", "apply_patch", vec!["src/a.rs".into()]);
        stale.record_verify("v1", "cargo test", 0);
        stale.record_mutation("m2", "apply_patch", vec!["src/b.rs".into()]);
        append(
            &events,
            &session,
            EngineEvent::EvidenceLedgerUpdated { ledger: stale },
        )
        .await;
        let projected = project_goal_checkpoint(&events, &messages, &goal(), &session, None)
            .await
            .unwrap();
        assert_eq!(
            projected.payload.verification,
            CheckpointVerification::Unmeasured,
            "a stale pass is not current-state evidence"
        );
    }

    #[tokio::test]
    async fn failed_verification_projects_as_failed() {
        let events = MemoryEventStore::new();
        let messages = MemoryMessageStore::new();
        let session = SessionId::new("s1");
        let mut ledger = EvidenceLedger::default();
        ledger.record_mutation("m1", "apply_patch", vec!["src/a.rs".into()]);
        ledger.record_verify("v1", "cargo test", 1);
        append(
            &events,
            &session,
            EngineEvent::EvidenceLedgerUpdated { ledger },
        )
        .await;
        let projected = project_goal_checkpoint(&events, &messages, &goal(), &session, None)
            .await
            .unwrap();
        match projected.payload.verification {
            CheckpointVerification::Failed { detail } => {
                assert!(detail.contains("cargo test"), "got: {detail}");
            }
            other => panic!("expected failed, got {other:?}"),
        }
    }

    /// Truth cases E/F: an incomplete child and a completed-no-findings
    /// child project distinctly, straight from the durable settlement facts.
    #[tokio::test]
    async fn child_truth_is_preserved() {
        let events = MemoryEventStore::new();
        let messages = MemoryMessageStore::new();
        let session = SessionId::new("s1");
        append(
            &events,
            &session,
            EngineEvent::SubAgentFinished {
                id: "c1".into(),
                nickname: "Explorer".into(),
                ok: true,
                summary: "done".into(),
                contribution: Some(leveler_lifecycle::ChildResultProjection {
                    child_id: "c1".into(),
                    role: "explorer".into(),
                    ..Default::default()
                }),
            },
        )
        .await;
        append(
            &events,
            &session,
            EngineEvent::SubAgentFinished {
                id: "c2".into(),
                nickname: "Reviewer".into(),
                ok: false,
                summary: "budget exhausted".into(),
                contribution: None,
            },
        )
        .await;

        let projected = project_goal_checkpoint(&events, &messages, &goal(), &session, None)
            .await
            .unwrap();
        let children = &projected.payload.children;
        assert_eq!(children.len(), 2);
        assert!(children[0].completed && children[0].contribution.is_some());
        assert!(
            !children[1].completed && children[1].contribution.is_none(),
            "incomplete-no-result must not read as completed-no-findings"
        );
    }

    /// Plan progress comes from the ledger's plan mirror.
    #[tokio::test]
    async fn plan_progress_is_projected() {
        let events = MemoryEventStore::new();
        let messages = MemoryMessageStore::new();
        let session = SessionId::new("s1");
        let ledger = EvidenceLedger {
            plan: leveler_lifecycle::PlanState {
                steps: vec![
                    leveler_lifecycle::PlanStep {
                        step: "audit".into(),
                        status: "completed".into(),
                        id: None,
                        origin: Default::default(),
                    },
                    leveler_lifecycle::PlanStep {
                        step: "implement".into(),
                        status: "pending".into(),
                        id: None,
                        origin: Default::default(),
                    },
                ],
            },
            ..Default::default()
        };
        append(
            &events,
            &session,
            EngineEvent::EvidenceLedgerUpdated { ledger },
        )
        .await;
        let projected = project_goal_checkpoint(&events, &messages, &goal(), &session, None)
            .await
            .unwrap();
        let plan = projected.payload.plan.expect("plan projected");
        assert_eq!((plan.completed, plan.total), (1, 2));
        assert_eq!(plan.next_step.as_deref(), Some("implement"));
    }
}
