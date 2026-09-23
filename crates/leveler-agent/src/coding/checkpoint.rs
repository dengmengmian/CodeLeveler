//! Coding's canonical GoalCheckpoint builder (long-goal P3).
//!
//! Every trigger — `/recap`, milestone, interruption, context compaction —
//! projects through this module, so a Recap in the TUI, the compaction
//! breadcrumb, and the resume context all present the SAME persisted facts.
//! Nothing here asks the model what the runtime already knows: structured
//! facts come from the event log, the evidence ledger, and whatever bounded
//! workspace metadata the Coding harness captures. The optional semantic
//! wording is applied by the caller on top and can fail without costing the
//! structured checkpoint.
//!
//! Cursor discipline: [`project_goal_checkpoint`] reads the committed
//! `MAX(sequence)` of the session's event log. The CALLER owns making that
//! read safe — every trigger sits behind a durable boundary (the event flush
//! barrier, a committed terminal turn, or the reaper's fenced commit), so
//! the cursor can never point beyond durable EventLog state.

use leveler_core::{GoalId, SessionId, TurnId};
use leveler_lifecycle::{
    CheckpointChild, CheckpointFindings, CheckpointPlan, CheckpointReason, CheckpointWorkspace,
    EvidenceLedger, GoalCheckpoint,
};
use leveler_storage::{
    EventRecord, EventStore, GoalCheckpointRecord, GoalRecord, GoalState, MessageStore,
};

use leveler_engine::{EngineError, EngineEvent, EventEmitter, PortError};

use crate::executor::CompactionCheckpoint;

/// Bounded facts about the Coding workspace at a checkpoint boundary.
#[async_trait::async_trait]
pub trait WorkspaceFacts: Send + Sync {
    async fn capture(&self) -> CheckpointWorkspace;
}

/// Exact identity and event boundary of one goal continuation lineage.
///
/// Callers obtain this from the turn payload. A Chat has no such scope and
/// therefore cannot install, create, or consume a GoalCheckpoint.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GoalCheckpointScope {
    pub goal_id: GoalId,
    pub root_turn_id: TurnId,
    pub turn_ids: Vec<TurnId>,
}

impl GoalCheckpointScope {
    pub fn new(goal_id: GoalId, root_turn_id: TurnId, mut turn_ids: Vec<TurnId>) -> Self {
        if !turn_ids.iter().any(|id| id == &root_turn_id) {
            turn_ids.push(root_turn_id.clone());
        }
        Self {
            goal_id,
            root_turn_id,
            turn_ids,
        }
    }

    fn contains_event(&self, event: &EventRecord) -> bool {
        event.turn_id.as_deref().is_some_and(|turn_id| {
            self.turn_ids
                .iter()
                .any(|candidate| candidate.as_str() == turn_id)
        })
    }
}

/// Resolve the newest persisted v2 turn lineage for one exact goal.
///
/// This is the only compatibility seam for out-of-band checkpoint triggers
/// such as `/recap` and restart reaping. It never guesses from session text,
/// goal state, or a v1 initiating payload. `None` means no authoritative v2
/// goal identity exists. Malformed lineage references fail closed.
pub async fn latest_goal_checkpoint_scope(
    stores: &leveler_storage::EngineStores,
    session_id: &SessionId,
    goal_id: &GoalId,
) -> Result<Option<GoalCheckpointScope>, EngineError> {
    let Some(task_id) = stores.tasks.task_for_session(session_id).await? else {
        return Ok(None);
    };
    let Some(goal) = stores.goals.get(goal_id).await? else {
        return Ok(None);
    };
    if goal.task_id != task_id {
        return Err(EngineError::Corrupt(format!(
            "goal {goal_id} does not belong to session {session_id}"
        )));
    }

    let turns = stores.turns.list_for_session(session_id).await?;
    let mut latest = None;
    for turn in turns.iter().rev() {
        let Some(payload) = turn.payload.as_deref() else {
            continue;
        };
        let version = serde_json::from_str::<serde_json::Value>(payload)
            .ok()
            .and_then(|value| value.get("version")?.as_u64());
        if version != Some(2) {
            continue;
        }
        let decoded = leveler_engine::decode_turn_continuation(payload)?;
        if decoded.goal_id.as_ref() == Some(goal_id) {
            latest = Some((turn, decoded));
            break;
        }
    }
    let Some((latest_turn, decoded)) = latest else {
        return Ok(None);
    };
    if latest_turn.kind == "chat" {
        return Err(EngineError::Corrupt(format!(
            "chat turn {} cannot name goal {goal_id}",
            latest_turn.id
        )));
    }
    let root_turn_id = decoded
        .root_turn_id
        .unwrap_or_else(|| TurnId::new(latest_turn.id.clone()));
    let root = turns
        .iter()
        .find(|turn| turn.id == root_turn_id.as_str())
        .ok_or_else(|| {
            EngineError::Corrupt(format!(
                "goal {goal_id} continuation names missing root {root_turn_id}"
            ))
        })?;
    if root.ordinal > latest_turn.ordinal {
        return Err(EngineError::Corrupt(format!(
            "goal {goal_id} continuation root {root_turn_id} is newer than turn {}",
            latest_turn.id
        )));
    }
    if root.kind != "user" {
        return Err(EngineError::Corrupt(format!(
            "goal {goal_id} continuation root {root_turn_id} has kind {}",
            root.kind
        )));
    }
    if let Some(root_payload) = root.payload.as_deref() {
        let root_version = serde_json::from_str::<serde_json::Value>(root_payload)
            .ok()
            .and_then(|value| value.get("version")?.as_u64());
        if root_version == Some(2) {
            let root_decoded = leveler_engine::decode_turn_continuation(root_payload)?;
            if root_decoded.goal_id.as_ref() != Some(goal_id) || root_decoded.root_turn_id.is_some()
            {
                return Err(EngineError::Corrupt(format!(
                    "turn {root_turn_id} is not the root of goal {goal_id}"
                )));
            }
        } else {
            // A v2 continuation is the authoritative upgrade seam for a v1
            // root: it supplies the exact goal identity the old row lacked.
            leveler_engine::decode_turn_initiating_message(root_payload)?;
        }
    }

    let mut turn_ids = Vec::new();
    for turn in &turns {
        let Some(payload) = turn.payload.as_deref() else {
            continue;
        };
        let version = serde_json::from_str::<serde_json::Value>(payload)
            .ok()
            .and_then(|value| value.get("version")?.as_u64());
        if version != Some(2) {
            continue;
        }
        let turn_lineage = leveler_engine::decode_turn_continuation(payload)?;
        if turn_lineage.goal_id.as_ref() == Some(goal_id)
            && turn.kind != "chat"
            && (turn.id == root_turn_id.as_str()
                || turn_lineage.root_turn_id.as_ref() == Some(&root_turn_id))
        {
            turn_ids.push(TurnId::new(turn.id.clone()));
        }
    }
    Ok(Some(GoalCheckpointScope::new(
        goal_id.clone(),
        root_turn_id,
        turn_ids,
    )))
}

/// Resolve the current session's exact Goal checkpoint scope for host actions
/// that do not already carry a goal id.
///
/// Only the latest user/chat turn may identify the active work. A legacy v1
/// turn or a v2 Chat (`goal_id = None`) returns `None`; this deliberately does
/// not walk backward into an older Goal.
pub async fn latest_session_goal_checkpoint_scope(
    stores: &leveler_storage::EngineStores,
    session_id: &SessionId,
) -> Result<Option<GoalCheckpointScope>, EngineError> {
    let turns = stores.turns.list_for_session(session_id).await?;
    let Some(turn) = turns
        .iter()
        .rev()
        .find(|turn| turn.kind == "user" || turn.kind == "chat")
    else {
        return Ok(None);
    };
    let Some(payload) = turn.payload.as_deref() else {
        return Ok(None);
    };
    let version = serde_json::from_str::<serde_json::Value>(payload)
        .ok()
        .and_then(|value| value.get("version")?.as_u64());
    if version != Some(2) {
        return Ok(None);
    }
    let decoded = leveler_engine::decode_turn_continuation(payload)?;
    let Some(goal_id) = decoded.goal_id else {
        return Ok(None);
    };
    latest_goal_checkpoint_scope(stores, session_id, &goal_id).await
}

pub(crate) struct CodingCompactionCheckpoint {
    engine: leveler_engine::TaskEngine,
    session_id: SessionId,
    scope: GoalCheckpointScope,
    workspace: Option<std::sync::Arc<dyn WorkspaceFacts>>,
    events: EventEmitter,
}

pub struct CodingCheckpointContext {
    pub engine: leveler_engine::TaskEngine,
    pub workspace: Option<std::sync::Arc<dyn WorkspaceFacts>>,
}

impl CodingCheckpointContext {
    pub fn new(
        engine: leveler_engine::TaskEngine,
        workspace: Option<std::sync::Arc<dyn WorkspaceFacts>>,
    ) -> Self {
        Self { engine, workspace }
    }
}

impl CodingCompactionCheckpoint {
    pub(crate) fn new(
        engine: leveler_engine::TaskEngine,
        session_id: SessionId,
        scope: GoalCheckpointScope,
        workspace: Option<std::sync::Arc<dyn WorkspaceFacts>>,
        events: EventEmitter,
    ) -> Self {
        Self {
            engine,
            session_id,
            scope,
            workspace,
            events,
        }
    }
}

#[async_trait::async_trait]
impl CompactionCheckpoint for CodingCompactionCheckpoint {
    async fn checkpoint_before_compaction(
        &self,
        summary: Option<&str>,
    ) -> Result<Option<String>, PortError> {
        self.events.flush().await.map_err(PortError::Persistence)?;
        let record = create_goal_checkpoint(
            &self.engine,
            &self.session_id,
            &self.scope,
            CheckpointReason::ContextCompaction,
            self.workspace.as_deref(),
            SemanticRecap::briefing(summary),
        )
        .await
        .map_err(|e| PortError::Persistence(e.to_string()))?;
        let Some(record) = record else {
            return Ok(None);
        };
        self.events.emit(checkpoint_created_event(&record));
        Ok(Some(record.payload.context_block()))
    }
}

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
    scope: &GoalCheckpointScope,
    workspace: Option<&dyn WorkspaceFacts>,
) -> Result<ProjectedCheckpoint, EngineError> {
    if goal.id != scope.goal_id {
        return Err(EngineError::Config(format!(
            "checkpoint scope names goal {}, but projection received {}",
            scope.goal_id, goal.id
        )));
    }
    let event_cursor = events.latest_sequence(session_id).await?.unwrap_or(0);
    let transcript_ordinal = messages.load(session_id).await?.len() as u64;

    let ledger = last_ledger(events, session_id, scope).await?;
    let findings = match &ledger {
        Some(ledger) => findings_from(ledger),
        // The ledger could not be read / was never written: explicitly
        // unknown — never zero.
        None => CheckpointFindings::Unknown,
    };
    let plan = match &ledger {
        Some(ledger) => CheckpointPlan::from_state(&ledger.plan),
        None => last_plan(events, session_id, scope)
            .await?
            .as_ref()
            .and_then(CheckpointPlan::from_state),
    };

    let payload = GoalCheckpoint {
        objective: goal.objective.clone(),
        lineage_root_turn_id: Some(scope.root_turn_id.as_str().to_string()),
        transcript_ordinal: Some(transcript_ordinal),
        plan,
        findings,
        children: settled_children(events, session_id, scope).await?,
        artifact_refs: Vec::new(),
        workspace: match workspace {
            Some(facts) => facts.capture().await,
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

/// Resolve the explicitly named goal, project, and PERSIST a checkpoint. The one
/// creation seam every trigger calls.
///
/// A running goal is accepted; a settled one is accepted only for `Manual`
/// (a user may ask for a recap after the run ended). `semantic_summary`, when
/// present, becomes the payload's goal summary — structured facts never
/// depend on it.
///
/// The caller owns the durable barrier BEFORE this call (event flush /
/// committed terminal / reaper commit), so the captured cursor is
/// committed-only by construction.
pub async fn create_goal_checkpoint(
    engine: &leveler_engine::TaskEngine,
    session_id: &SessionId,
    scope: &GoalCheckpointScope,
    reason: CheckpointReason,
    workspace: Option<&dyn WorkspaceFacts>,
    semantic: Option<SemanticRecap>,
) -> Result<Option<GoalCheckpointRecord>, EngineError> {
    let stores = &engine.stores;
    let Some(task_id) = stores.tasks.task_for_session(session_id).await? else {
        return Err(EngineError::Config(format!(
            "cannot checkpoint goal {}: session {session_id} has no task",
            scope.goal_id
        )));
    };
    let Some(goal) = stores.goals.get(&scope.goal_id).await? else {
        return Err(EngineError::Config(format!(
            "cannot checkpoint missing goal {}",
            scope.goal_id
        )));
    };
    if goal.task_id != task_id {
        return Err(EngineError::Config(format!(
            "goal {} does not belong to session {session_id}",
            scope.goal_id
        )));
    }
    if goal.state != GoalState::Running && !matches!(reason, CheckpointReason::Manual) {
        return Ok(None);
    }
    let projected = project_goal_checkpoint(
        stores.events.as_ref(),
        stores.messages.as_ref(),
        &goal,
        session_id,
        scope,
        workspace,
    )
    .await?;
    let mut payload = projected.payload;
    if let Some(semantic) = semantic {
        payload.goal_summary = semantic.goal_summary;
        payload.display_summary = semantic.display_summary;
        payload.next_action = semantic.next_action;
    }
    let record = engine
        .commit_goal_checkpoint(leveler_storage::NewGoalCheckpoint {
            goal_id: scope.goal_id.clone(),
            session_id: session_id.clone(),
            reason,
            event_cursor: projected.event_cursor,
            payload: payload.bounded(),
        })
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
/// The transcript watermark of the checkpoint a resume would continue from,
/// when there is one. A cheap indexed probe: it answers "how far back can the
/// checkpoint path reach" without loading a message, which is what lets a
/// bounded transcript load know it is safe.
///
/// `None` covers every case where the checkpoint path will not fire or its
/// reach is unknown — no task, no goal, no checkpoint, no watermark — and the
/// caller reads that as "impose no bound", never as "bound at zero".
pub async fn checkpoint_transcript_ordinal(
    stores: &leveler_storage::EngineStores,
    session_id: &SessionId,
    scope: &GoalCheckpointScope,
) -> Result<Option<u64>, EngineError> {
    let Some(checkpoint) = latest_checkpoint_for_scope(stores, session_id, scope).await? else {
        return Ok(None);
    };
    Ok(checkpoint.payload.transcript_ordinal)
}

pub async fn resume_prior_from_checkpoint(
    stores: &leveler_storage::EngineStores,
    session_id: &SessionId,
    scope: &GoalCheckpointScope,
    transcript: &leveler_engine::RawTranscript,
) -> Result<Option<Vec<leveler_model::Message>>, EngineError> {
    let Some(checkpoint) = latest_checkpoint_for_scope(stores, session_id, scope).await? else {
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
    // The delta this checkpoint stands in front of. `None` means the loaded
    // transcript cannot serve it — the watermark is past its end, or a bounded
    // load began after it. Falling back to full history is the safe answer;
    // splicing a shorter delta would silently drop work the checkpoint does
    // not describe.
    let Some(delta) = transcript.slice_from_ordinal(ordinal) else {
        tracing::warn!(
            checkpoint = %checkpoint.id,
            ordinal,
            transcript_offset = transcript.offset(),
            transcript_len = transcript.stored_len(),
            "checkpoint watermark is outside the loaded transcript; falling back"
        );
        return Ok(None);
    };
    let mut prior = Vec::with_capacity(1 + delta.len());
    prior.push(leveler_model::Message {
        role: leveler_model::Role::User,
        content: vec![leveler_model::ContentPart::Text {
            text: checkpoint.payload.context_block(),
        }],
    });
    prior.extend_from_slice(delta);
    Ok(Some(prior))
}

async fn latest_checkpoint_for_scope(
    stores: &leveler_storage::EngineStores,
    session_id: &SessionId,
    scope: &GoalCheckpointScope,
) -> Result<Option<GoalCheckpointRecord>, EngineError> {
    Ok(stores
        .goal_checkpoints
        .for_goal(&scope.goal_id)
        .await?
        .into_iter()
        .find(|checkpoint| checkpoint_matches_scope(checkpoint, session_id, scope)))
}

fn checkpoint_matches_scope(
    checkpoint: &GoalCheckpointRecord,
    session_id: &SessionId,
    scope: &GoalCheckpointScope,
) -> bool {
    checkpoint.goal_id == scope.goal_id
        && checkpoint.session_id == *session_id
        && checkpoint.payload.lineage_root_turn_id.as_deref() == Some(scope.root_turn_id.as_str())
}

async fn last_ledger(
    events: &dyn EventStore,
    session_id: &SessionId,
    scope: &GoalCheckpointScope,
) -> Result<Option<EvidenceLedger>, EngineError> {
    let Some(row) = events
        .load_by_types(session_id, &["evidence_ledger_updated"])
        .await?
        .into_iter()
        .rev()
        .find(|row| scope.contains_event(row))
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
    scope: &GoalCheckpointScope,
) -> Result<Option<leveler_lifecycle::PlanState>, EngineError> {
    let Some(row) = events
        .load_by_types(session_id, &["plan_updated"])
        .await?
        .into_iter()
        .rev()
        .find(|row| scope.contains_event(row))
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
    CheckpointFindings::Known {
        total: ledger.findings.len() as u32,
        refs: ledger
            .findings
            .iter()
            .take(MAX_REFS)
            .map(|f| f.id.clone())
            .collect(),
    }
}

async fn settled_children(
    events: &dyn EventStore,
    session_id: &SessionId,
    scope: &GoalCheckpointScope,
) -> Result<Vec<CheckpointChild>, EngineError> {
    let rows = events
        .load_by_types(session_id, &["sub_agent_finished"])
        .await?;
    let mut out = Vec::new();
    for row in rows.into_iter().filter(|row| scope.contains_event(row)) {
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

    fn scope() -> GoalCheckpointScope {
        GoalCheckpointScope::new(
            GoalId::new("g1"),
            TurnId::new("root"),
            vec![TurnId::new("root")],
        )
    }

    async fn append(events: &MemoryEventStore, session: &SessionId, event: EngineEvent) {
        let (event_type, payload) = event.to_row().unwrap();
        events
            .append(
                session,
                Some(&TurnId::new("root")),
                &event_type,
                &payload,
                leveler_core::now(),
            )
            .await
            .unwrap();
    }

    fn finding(id: &str) -> FindingRecord {
        FindingRecord {
            id: id.to_string(),
            source_child: "c1".to_string(),
            role: "explorer".to_string(),
            kind: FindingKind::Correctness,
            summary: format!("finding {id}"),
            file: None,
            symbol: None,
        }
    }

    /// With no ledger written, findings are UNKNOWN rather than zero.
    #[tokio::test]
    async fn no_ledger_projects_unknown_not_success() {
        let events = MemoryEventStore::new();
        let messages = MemoryMessageStore::new();
        let session = SessionId::new("s1");
        let projected =
            project_goal_checkpoint(&events, &messages, &goal(), &session, &scope(), None)
                .await
                .unwrap();
        assert_eq!(projected.payload.findings, CheckpointFindings::Unknown);
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
        let projected =
            project_goal_checkpoint(&events, &messages, &goal(), &session, &scope(), None)
                .await
                .unwrap();
        assert_eq!(projected.event_cursor, 3);
    }

    /// Truth case D: every recorded finding is counted and referenced. There
    /// is no "open" or "blocking" subset any more — a finding is information.
    #[tokio::test]
    async fn findings_truth_is_preserved() {
        let events = MemoryEventStore::new();
        let messages = MemoryMessageStore::new();
        let session = SessionId::new("s1");
        let ledger = EvidenceLedger {
            findings: vec![finding("f-1"), finding("f-2"), finding("f-3")],
            ..Default::default()
        };
        append(
            &events,
            &session,
            EngineEvent::EvidenceLedgerUpdated { ledger },
        )
        .await;

        let projected =
            project_goal_checkpoint(&events, &messages, &goal(), &session, &scope(), None)
                .await
                .unwrap();
        match projected.payload.findings {
            CheckpointFindings::Known { total, refs } => {
                assert_eq!(total, 3);
                assert_eq!(
                    refs,
                    vec!["f-1".to_string(), "f-2".to_string(), "f-3".to_string()]
                );
            }
            other => panic!("expected known findings, got {other:?}"),
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
                outcome: None,
                stop: None,
                limit: None,
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
                outcome: None,
                stop: None,
                limit: None,
            },
        )
        .await;

        let projected =
            project_goal_checkpoint(&events, &messages, &goal(), &session, &scope(), None)
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
        let projected =
            project_goal_checkpoint(&events, &messages, &goal(), &session, &scope(), None)
                .await
                .unwrap();
        let plan = projected.payload.plan.expect("plan projected");
        assert_eq!((plan.completed, plan.total), (1, 2));
        assert_eq!(plan.next_step.as_deref(), Some("implement"));
    }

    /// A checkpoint projects only the turns in its continuation lineage. An
    /// older goal in the same session must not donate its plan or evidence.
    #[tokio::test]
    async fn projection_ignores_events_from_a_foreign_lineage() {
        let events = MemoryEventStore::new();
        let messages = MemoryMessageStore::new();
        let session = SessionId::new("s1");
        let foreign = TurnId::new("old-ci-turn");
        let plan = EngineEvent::PlanUpdated {
            steps: vec![leveler_lifecycle::PlanStep {
                step: "delete failed CI runs".into(),
                status: "pending".into(),
                id: None,
                origin: Default::default(),
            }],
        };
        let (event_type, payload) = plan.to_row().unwrap();
        events
            .append(
                &session,
                Some(&foreign),
                &event_type,
                &payload,
                leveler_core::now(),
            )
            .await
            .unwrap();

        let projected =
            project_goal_checkpoint(&events, &messages, &goal(), &session, &scope(), None)
                .await
                .unwrap();

        assert!(projected.payload.plan.is_none());
        assert_eq!(
            projected.payload.lineage_root_turn_id.as_deref(),
            Some("root")
        );
    }
}
