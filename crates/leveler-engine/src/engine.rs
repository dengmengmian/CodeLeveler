//! The task engine: session and task lifecycle.
//!
//! `create_task` persists WHAT will run (goal/mode/sandbox/kind) so resume
//! never guesses; ownership, the running transition and the terminal columns
//! are all written here, and nowhere else.
//!
//! Authority boundary: the engine records how a run ended and stamps what it
//! was told about the final tree. It does not decide when a task is done, does
//! not judge whether the work satisfies the user, and does not know what kind
//! of agent produced it.

use leveler_core::{GoalId, SessionId, TaskId, TurnId};
use leveler_lifecycle::{AgentState, SessionStatus, StopReason};
use leveler_storage::{EngineStores, EventStore, SessionRecord};

use crate::log::{DanglingCall, EventLog, SnapshotView};
use crate::{EngineError, EngineEvent, ExecutionKind, TaskOutcome};

/// Bound prior messages for a model request.
///
/// **Under threshold:** always use full `raw` from MessageRepository — a
/// ContextSnapshot is never a permanent replacement for later turns.
/// **Over threshold:** merge snapshot (compact base) with the raw tail that
/// arrived after the snapshot was taken, then fold if still oversized. A
/// snapshot with a `through_ordinal` watermark appends exactly `raw[n..]`;
/// only watermark-less legacy snapshots use suffix-overlap inference.
///
/// `summary` is a handoff briefing for the fold; callers that can produce
/// one lazily go through [`crate::RawTranscript::assemble`], which asks for
/// it only when the merged base is still over `threshold`.
///
/// Returns `(messages_for_model, wrote_compact)` — `wrote_compact` means the
/// caller should persist a new ContextSnapshot.
pub fn budget_prior_messages(
    raw: Vec<leveler_model::Message>,
    snapshot: Option<SnapshotView>,
    summary: Option<&str>,
    active_objective: Option<&str>,
    threshold: u64,
) -> (Vec<leveler_model::Message>, bool) {
    match merge_prior_messages(raw, 0, snapshot, threshold) {
        (base, PriorMerge::Fits { merged }) => (base, merged),
        (base, PriorMerge::Over { base_tokens }) => {
            fold_prior_messages(base, base_tokens, summary, active_objective, threshold)
        }
    }
}

/// What [`merge_prior_messages`] found: the merged base fits (and whether a
/// snapshot was merged into it, i.e. a shorter snapshot is worth persisting),
/// or it is still over threshold and must be folded.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PriorMerge {
    Fits { merged: bool },
    Over { base_tokens: u64 },
}

/// Step one of [`budget_prior_messages`]: the raw transcript under threshold,
/// or the latest snapshot merged with its post-snapshot tail. No model call
/// is needed to get here, so a caller decides whether a summary is worth
/// producing only after seeing `PriorMerge::Over`.
pub(crate) fn merge_prior_messages(
    raw: Vec<leveler_model::Message>,
    // Absolute ordinal `raw[0]` sits at. Non-zero when the caller loaded only
    // the reachable tail; the snapshot watermark is absolute, so it must be
    // rebased before it indexes `raw`.
    raw_offset: u64,
    snapshot: Option<SnapshotView>,
    threshold: u64,
) -> (Vec<leveler_model::Message>, PriorMerge) {
    let raw_tokens = leveler_context::estimate_tokens(&raw);
    if raw_tokens <= threshold {
        return (raw, PriorMerge::Fits { merged: false });
    }

    let base = match snapshot {
        Some(view) if !snapshot_is_usable(&view) => raw,
        Some(view) if !view.messages.is_empty() => match view.through_ordinal {
            Some(n) => match n.checked_sub(raw_offset) {
                // Exact watermark: everything after transcript ordinal `n`
                // post-dates the snapshot. The tail is appended as-is: a join
                // that opens on a result the snapshot does not own is refused
                // at the provider boundary, never repaired here.
                Some(local) if local as usize <= raw.len() => {
                    let mut out = view.messages;
                    out.extend_from_slice(&raw[local as usize..]);
                    out
                }
                // A watermark beyond the live transcript means the transcript
                // was truncated after the snapshot (context ops normally
                // rewrite the snapshot too). Never guess a slice: fall back
                // to the legacy overlap merge and say so.
                Some(_) => {
                    tracing::warn!(
                        through_ordinal = n,
                        raw_offset,
                        raw_len = raw.len(),
                        "context snapshot watermark exceeds transcript; using overlap merge"
                    );
                    merge_snapshot_with_raw_tail(view.messages, &raw)
                }
                // The snapshot predates this load: the rows between its
                // watermark and the load's start were never read. Never guess
                // a slice here either.
                None => {
                    tracing::warn!(
                        through_ordinal = n,
                        raw_offset,
                        "context snapshot watermark precedes the loaded transcript; using overlap merge"
                    );
                    merge_snapshot_with_raw_tail(view.messages, &raw)
                }
            },
            None => merge_snapshot_with_raw_tail(view.messages, &raw),
        },
        _ => raw,
    };
    let tokens = leveler_context::estimate_tokens(&base);
    if tokens <= threshold {
        // Snapshot+tail already fits: persist so next request starts shorter.
        return (base, PriorMerge::Fits { merged: true });
    }
    (
        base,
        PriorMerge::Over {
            base_tokens: tokens,
        },
    )
}

/// Step two of [`budget_prior_messages`]: fold a merged base that is still
/// over threshold.
pub(crate) fn fold_prior_messages(
    base: Vec<leveler_model::Message>,
    base_tokens: u64,
    summary: Option<&str>,
    active_objective: Option<&str>,
    threshold: u64,
) -> (Vec<leveler_model::Message>, bool) {
    // HCH-FIX-2: bound the retained tail by TOKENS as well as by count —
    // half the fold threshold, mirroring the agent loop's `budget / 2`
    // (drive loop passes `current_budget / 2`). With `0` here, a single
    // huge tool result inside the last 12 messages rode through a
    // 24k-threshold fold intact, retaining 5-19x the threshold.
    let folded = leveler_context::compact_messages(
        &base,
        leveler_context::COMPACT_KEEP_RECENT,
        threshold / 2,
        summary,
        active_objective,
    );
    let changed =
        leveler_context::estimate_tokens(&folded) < base_tokens || folded.len() < base.len();
    (folded, changed || base_tokens > threshold)
}

/// Append raw messages that post-date the snapshot. Snapshot is often a
/// compacted view (summary + recent window), so we locate the longest suffix of
/// `snap` that appears as a contiguous slice of `raw` and keep everything after.
fn merge_snapshot_with_raw_tail(
    snap: Vec<leveler_model::Message>,
    raw: &[leveler_model::Message],
) -> Vec<leveler_model::Message> {
    if raw.is_empty() {
        return snap;
    }
    let snap_len = snap.len();
    let max_k = snap_len.min(raw.len());
    for k in (1..=max_k).rev() {
        let suffix = &snap[snap_len - k..];
        // Search from the end so we match the most recent occurrence.
        for i in (0..=raw.len() - k).rev() {
            if messages_slice_eq(suffix, &raw[i..i + k]) {
                let mut out = snap;
                out.extend_from_slice(&raw[i + k..]);
                return out;
            }
        }
    }
    // No overlap (pure summary snapshot): keep snap + trailing raw window,
    // widened to the round boundary so the window never opens on a result.
    let keep = leveler_context::COMPACT_KEEP_RECENT.min(raw.len());
    let start = leveler_context::round_boundary(raw, raw.len() - keep);
    let mut out = snap;
    out.extend_from_slice(&raw[start..]);
    out
}

/// Whether a persisted snapshot may stand in for the transcript it was cut
/// from. A snapshot is a derived view; one that breaks the tool-exchange
/// invariant (a pre-fix merge wrote such views) would make every later request
/// fail, while the transcript it came from is intact. Such a snapshot is
/// ignored — the same path as having no snapshot — and says so.
///
/// The snapshot is a prefix: an exchange still open at its end is answered by
/// the tail it is joined to, and the joined request is validated as a whole at
/// the provider boundary.
pub(crate) fn snapshot_is_usable(view: &SnapshotView) -> bool {
    use leveler_model::ToolExchangeViolation::UnansweredCall;
    let messages = &view.messages;
    match leveler_model::validate_tool_exchange(messages) {
        Ok(()) => true,
        Err(UnansweredCall { index, .. })
            if messages[index + 1..]
                .iter()
                .all(|m| m.role == leveler_model::Role::Tool) =>
        {
            true
        }
        Err(violation) => {
            tracing::warn!(
                %violation,
                through_ordinal = view.through_ordinal,
                "ignoring a context snapshot that breaks the tool-exchange invariant"
            );
            false
        }
    }
}

fn messages_slice_eq(a: &[leveler_model::Message], b: &[leveler_model::Message]) -> bool {
    a.len() == b.len() && a.iter().zip(b.iter()).all(|(x, y)| x == y)
}

/// The persistent task engine.
///
/// Persistence enters exclusively through [`EngineStores`] — narrow
/// capability ports the composition root wires to its adapter (SQLite
/// locally). The engine never names a concrete database.
#[derive(Clone)]
pub struct TaskEngine {
    pub stores: EngineStores,
    /// This runtime's durable identity (from the composition root). Task
    /// ownership is acquired for it at every execution entry.
    pub runtime_id: leveler_core::RuntimeId,
    /// The live boot this engine executes as. Several boots can share one
    /// runtime identity; ownership and recovery decide by boot, never by
    /// runtime identity alone.
    pub boot: EngineBoot,
}

/// A boot as the engine sees it: its id, and the host's answer to whether
/// some other boot is still alive.
#[derive(Clone)]
pub struct EngineBoot {
    pub id: leveler_core::BootId,
    pub liveness: std::sync::Arc<dyn leveler_core::BootLivenessProbe>,
}

/// The persisted description of a session to create.
///
/// Every field is a durable value the engine writes and never reads back for
/// a decision: which workspace, which model, which permission mode. What they
/// MEAN belongs to the harness that chose them.
pub struct NewSession {
    /// The workspace the session is about, as it should be persisted.
    pub workspace: String,
    pub goal: String,
    pub model: String,
    /// The permission mode's wire string.
    pub mode: String,
    pub sandbox: bool,
    pub kind: ExecutionKind,
    /// Product-defined axes persisted opaquely with the session. The engine
    /// does not interpret either value.
    pub axes: Option<NewSessionAxes>,
}

pub struct NewSessionAxes {
    pub collaboration: String,
    pub work_profile: String,
}

/// Harness-supplied execution configuration written at a fenced start.
pub struct TaskExecution {
    pub mode: String,
    pub sandbox: bool,
    pub kind: ExecutionKind,
}

/// Harness-supplied terminal facts for one task.
///
/// The engine persists these values atomically. It does not derive the
/// outcome, workflow state, or goal disposition.
pub struct TaskTerminal {
    /// How the harness says the task ended.
    pub outcome: TaskOutcome,
    /// Optional terminal detail for a non-success outcome.
    pub reason: Option<String>,
    /// The structured provider failure behind a failed outcome, when there was
    /// one. Persisted with the terminal fact so live and replay reach the same
    /// structured presentation.
    pub failure: Option<leveler_model::ModelError>,
    /// Optional executor stop reason.
    pub stop: Option<StopReason>,
    /// Durable session status selected by the harness.
    pub status: SessionStatus,
    /// Durable domain workflow state selected by the harness.
    pub state: AgentState,
    /// Optional long-goal projection committed with the terminal fact.
    pub goal: Option<leveler_storage::GoalTerminalUpdate>,
    /// Completion-contract warnings.
    pub warnings: Vec<String>,
}

impl TaskEngine {
    /// Commit the canonical terminal event and every session lifecycle column
    /// (outcome + status + state) atomically, then forward the event. The same
    /// commit ends the execution's ownership of the task: the boot that ran it
    /// may stay open, and the next execution — from any boot — acquires anew.
    /// The engine is the one writer of the lifecycle for every session it runs —
    /// no app layer stamps a second copy there — and an observer can never see
    /// an uncommitted fact. Normal, daemon, and parallel-parent lifecycles all
    /// pass through this same authority boundary.
    pub async fn finish_task(
        &self,
        token: &leveler_core::OwnershipToken,
        session_id: &SessionId,
        terminal: TaskTerminal,
        observer: &mut (dyn FnMut(EngineEvent) + Send),
    ) -> Result<(), EngineError> {
        let TaskTerminal {
            outcome,
            reason,
            failure,
            stop,
            status,
            state,
            goal,
            warnings,
        } = terminal;
        let event = EngineEvent::TaskFinished {
            outcome,
            reason,
            failure,
            stop,
            warnings,
        };
        let (event_type, payload) = event.to_row()?;
        let commit = self
            .stores
            .terminal
            .finish_task_owned(
                token,
                session_id,
                &event_type,
                &payload,
                outcome,
                status,
                state,
                goal.as_ref(),
                leveler_core::now(),
            )
            .await
            .map_err(|error| EngineError::TerminalCommitFailed(error.to_string()))?;
        if commit.inserted {
            observer(event);
        }
        Ok(())
    }

    /// Open a durable goal under the caller's current task ownership.
    /// Objective matching is harness semantics; the engine only authorizes
    /// and forwards the write.
    pub async fn open_goal(
        &self,
        token: &leveler_core::OwnershipToken,
        objective: &str,
    ) -> Result<GoalId, EngineError> {
        Ok(self
            .stores
            .goals
            .open(token, objective, leveler_core::now())
            .await?)
    }

    /// Persist a caller-projected checkpoint. The engine owns the mechanical
    /// write and does not inspect the domain payload.
    pub async fn commit_goal_checkpoint(
        &self,
        checkpoint: leveler_storage::NewGoalCheckpoint,
    ) -> Result<leveler_storage::GoalCheckpointRecord, EngineError> {
        Ok(self
            .stores
            .goal_checkpoints
            .create(checkpoint, leveler_core::now())
            .await?)
    }

    /// Acquire (or same-boot reacquire) ownership of the session's task.
    /// A task owned by a DIFFERENT runtime is a hard conflict - never
    /// auto-stolen. Within this runtime, a task another boot owns passes only
    /// once that boot is proven dead: a live one keeps it, and one whose
    /// liveness cannot be established keeps it too. A task whose last
    /// execution committed its terminal is unowned and passes to any boot.
    /// The epoch always advances, fencing prior incarnations.
    pub async fn acquire_ownership(
        &self,
        session_id: &SessionId,
    ) -> Result<leveler_core::OwnershipToken, EngineError> {
        let task_id = self
            .stores
            .tasks
            .ensure_for_session(session_id, leveler_core::now())
            .await?;
        // Acquire (or same-runtime reacquire) ownership BEFORE any
        // authoritative write. A task owned by another runtime is a hard
        // conflict — never auto-stolen; the CAS itself refuses concurrent
        // racers. The epoch always advances, so tokens from this runtime's
        // previous incarnation become stale here.
        let current = self
            .stores
            .ownership
            .current(&task_id)
            .await?
            .ok_or_else(|| EngineError::Config(format!("no task row for session {session_id}")))?;
        // A turn left running before boots were recorded has no boot whose
        // death could be proven; whoever runs it may still be running it.
        if self
            .stores
            .turns
            .list_running(Some(session_id))
            .await?
            .iter()
            .any(|turn| turn.owner_boot_id.is_none())
        {
            return Err(EngineError::OwnershipUnknown { task_id });
        }
        if let Some(refusal) = self.owner_refusal(&task_id, &current) {
            return Err(refusal);
        }
        match self
            .stores
            .ownership
            .acquire(&task_id, &self.runtime_id, &self.boot.id, current.epoch)
            .await
        {
            Ok(token) => Ok(token),
            // Losing the compare-and-swap means someone acquired first. Say
            // who, by the same rules; a change no refusal describes — this
            // boot racing itself, a winner already gone — stays stale.
            Err(error @ leveler_storage::OwnershipError::Stale { .. }) => {
                let winner = self.stores.ownership.current(&task_id).await?;
                Err(winner
                    .and_then(|winner| self.owner_refusal(&task_id, &winner))
                    .unwrap_or(EngineError::Ownership(error)))
            }
            Err(error) => Err(error.into()),
        }
    }

    /// Why this boot may not take a task from `owner`, if it may not: another
    /// runtime holds it, or another boot that is alive or cannot be probed.
    fn owner_refusal(
        &self,
        task_id: &leveler_core::TaskId,
        owner: &leveler_storage::TaskOwner,
    ) -> Option<EngineError> {
        if let Some(runtime) = &owner.runtime
            && runtime != &self.runtime_id
        {
            return Some(EngineError::OwnershipConflict {
                task_id: task_id.clone(),
                owner: runtime.clone(),
                epoch: owner.epoch,
                this_runtime: self.runtime_id.clone(),
            });
        }
        let boot = owner.boot.as_ref().filter(|boot| *boot != &self.boot.id)?;
        match self.boot.liveness.liveness(boot) {
            // An ended boot never comes back, so this cannot turn stale
            // before the compare-and-swap.
            leveler_core::BootLiveness::Dead => None,
            leveler_core::BootLiveness::Alive => Some(EngineError::OwnedByLiveBoot {
                task_id: task_id.clone(),
            }),
            leveler_core::BootLiveness::Unknown => Some(EngineError::OwnershipUnknown {
                task_id: task_id.clone(),
            }),
        }
    }

    /// End an ownership generation whose work is over, when no task terminal
    /// commit carries the release. Fenced and idempotent: a generation that is
    /// no longer current — released already, or followed by a later one — has
    /// nothing left to release, and a later generation is never touched. Only
    /// a storage failure is an error.
    pub async fn release_ownership(
        &self,
        token: &leveler_core::OwnershipToken,
    ) -> Result<(), EngineError> {
        match self.stores.ownership.release(token).await {
            Ok(()) => Ok(()),
            Err(leveler_storage::OwnershipError::Stale { actual_epoch, .. }) => {
                tracing::debug!(
                    task = %token.task_id,
                    released = %token.owner_epoch,
                    current = %actual_epoch,
                    "ownership generation already ended"
                );
                Ok(())
            }
            Err(error) => Err(error.into()),
        }
    }

    /// Mark the session running before the first turn (fenced), acquiring
    /// ownership first — the ONE seam every execution entry passes through.
    pub async fn mark_running(
        &self,
        session_id: &SessionId,
        state: AgentState,
    ) -> Result<leveler_core::OwnershipToken, EngineError> {
        let token = self.acquire_ownership(session_id).await?;
        self.stores
            .sessions
            .update_status_owned(
                &token,
                session_id,
                SessionStatus::Running,
                state,
                leveler_core::now(),
            )
            .await?;
        Ok(token)
    }

    /// Acquire ownership, then atomically persist the execution configuration
    /// and Running projection. A foreign owner rejects the operation before
    /// any session field changes.
    pub async fn start_task(
        &self,
        session_id: &SessionId,
        state: AgentState,
        execution: &TaskExecution,
    ) -> Result<leveler_core::OwnershipToken, EngineError> {
        let token = self.acquire_ownership(session_id).await?;
        self.stores
            .sessions
            .start_execution_owned(
                &token,
                session_id,
                &execution.mode,
                execution.sandbox,
                execution.kind.as_str(),
                state,
                leveler_core::now(),
            )
            .await?;
        Ok(token)
    }

    /// The durable task owning `session_id`, if the association exists yet.
    /// (It is created at latest when the session first runs.)
    pub async fn task_for_session(
        &self,
        session_id: &SessionId,
    ) -> Result<Option<TaskId>, EngineError> {
        Ok(self.stores.tasks.task_for_session(session_id).await?)
    }

    /// Create and persist the session row, including its execution config,
    /// and the durable task row associated with it.
    pub async fn create_task(&self, session: &NewSession) -> Result<SessionId, EngineError> {
        let mut record = SessionRecord::new(
            session.workspace.clone(),
            session.goal.clone(),
            session.model.clone(),
            leveler_core::now(),
        );
        if let Some(axes) = &session.axes {
            record = record.with_axes(&axes.collaboration, &axes.work_profile);
        }
        let id = SessionId::new(record.id.clone());
        self.stores
            .task_creation
            .create_task(
                &record,
                &session.mode,
                session.sandbox,
                session.kind.as_str(),
            )
            .await?;
        Ok(id)
    }

    /// Load the transcript a request needs, reading only the tail when both
    /// watermarks prove the earlier rows unreachable. Falls back to the full
    /// load whenever either is unknown; see
    /// [`crate::session_context::RawTranscript::load_bounded`].
    pub async fn load_request_transcript(
        &self,
        session_id: &SessionId,
        checkpoint_ordinal: Option<u64>,
        strict: Option<&str>,
    ) -> Result<crate::RawTranscript, EngineError> {
        let threshold = leveler_context::PRE_REQUEST_COMPACT_THRESHOLD;
        // An unusable snapshot bounds nothing: the merge will not use it, so
        // the rows before its watermark are still reachable.
        let snapshot_ordinal = EventLog::new(self.stores.events.as_ref(), session_id.clone())
            .latest_context_snapshot(None)
            .await?
            .filter(snapshot_is_usable)
            .and_then(|view| view.through_ordinal);
        crate::RawTranscript::load_bounded(
            self.stores.messages.as_ref(),
            session_id,
            threshold,
            snapshot_ordinal,
            checkpoint_ordinal,
            strict,
        )
        .await
    }

    pub async fn record_recovery_skip(
        &self,
        log: &EventLog<'_>,
        call: &DanglingCall,
        turn_ref: Option<&TurnId>,
        reason: &str,
        observer: &mut (dyn FnMut(EngineEvent) + Send),
    ) -> Result<(), EngineError> {
        log.append(
            turn_ref,
            EngineEvent::ToolCallFinished {
                exit_code: None,
                stop: None,
                call_id: call.call_id.clone(),
                name: call.name.clone(),
                is_error: true,
                preview: reason.to_string(),
                agent_id: call.agent_id.clone(),
                applied_diff: None,
            },
            observer,
        )
        .await
    }
}

/// Close every dangling tool call of a session with an explicit
/// user-acknowledged marker (an errored `ToolCallFinished`, never a fake
/// success), so a resume blocked by `RecoveryConfirmationRequired` can
/// proceed. Nothing is replayed. Returns how many calls were closed.
pub async fn acknowledge_crash_window(
    events: &dyn EventStore,
    token: &leveler_core::OwnershipToken,
    session_id: &SessionId,
) -> Result<usize, EngineError> {
    // The reconciling markers are canonical recovery facts: fenced, so a
    // stale or non-owner runtime cannot rewrite crash-window history.
    let log = EventLog::new_owned(events, session_id.clone(), token.clone());
    let dangling = log.dangling_tool_calls().await?;
    let closed = dangling.len();
    for call in dangling {
        let turn_id = call.turn_id.as_ref().map(|t| TurnId::new(t.clone()));
        log.append(
            turn_id.as_ref(),
            EngineEvent::ToolCallFinished {
                exit_code: None,
                stop: None,
                call_id: call.call_id.clone(),
                name: call.name.clone(),
                is_error: true,
                preview: "user-acknowledged crash recovery: the interrupted call's outcome \
                          is unknown and the call was not replayed"
                    .to_string(),
                agent_id: call.agent_id.clone(),
                applied_diff: None,
            },
            &mut |_| {},
        )
        .await?;
    }
    Ok(closed)
}

#[cfg(test)]
mod multi_turn_session_tests {
    use super::*;
    use leveler_model::{ContentPart, Message, Role};

    fn msg(role: Role, text: &str) -> Message {
        Message::text(role, text)
    }

    fn assistant_call(id: &str) -> Message {
        Message {
            role: Role::Assistant,
            content: vec![ContentPart::ToolCall {
                call: leveler_model::ToolCall {
                    id: leveler_core::ToolCallId::new(id),
                    name: "read_file".into(),
                    arguments: serde_json::json!({}),
                },
            }],
        }
    }

    fn tool_result(id: &str) -> Message {
        Message {
            role: Role::Tool,
            content: vec![ContentPart::ToolResult {
                result: leveler_model::ToolResultContent {
                    call_id: leveler_core::ToolCallId::new(id),
                    content: "ok".into(),
                    is_error: false,
                },
            }],
        }
    }

    /// The incident join (session 26fc1890): the transcript was loaded from
    /// the snapshot's own watermark, so `raw[0]` IS ordinal `n`. The absolute
    /// watermark rebased by the offset appends the whole post-snapshot tail.
    #[test]
    fn a_bounded_load_joins_the_tail_at_the_absolute_watermark() {
        let raw = vec![
            msg(Role::User, "push it"),
            assistant_call("c1"),
            tool_result("c1"),
            msg(Role::Assistant, "after"),
        ];
        let snap = SnapshotView {
            messages: vec![msg(Role::User, "summary")],
            through_ordinal: Some(170),
        };
        let (out, _) = merge_prior_messages(raw.clone(), 170, Some(snap), 0);
        assert_eq!(out[1..], raw[..], "the whole tail follows the snapshot");
        leveler_model::validate_tool_exchange(&out).expect("pairs intact");
    }

    /// A join whose tail begins on a result the snapshot does not own is a
    /// broken input, not a boundary to smooth over: the result is kept, so the
    /// provider boundary refuses the request instead of sending a history
    /// with an unexplained hole.
    #[test]
    fn an_unpairable_join_is_not_silently_repaired() {
        let raw = vec![tool_result("c1"), msg(Role::Assistant, "after")];
        let snap = SnapshotView {
            messages: vec![msg(Role::User, "summary")],
            through_ordinal: Some(2),
        };
        let (out, _) = merge_prior_messages(raw, 2, Some(snap), 0);
        assert_eq!(out[1].role, Role::Tool, "nothing dropped: {out:?}");
        assert!(leveler_model::validate_tool_exchange(&out).is_err());
    }

    /// The watermark-less fallback keeps the last `COMPACT_KEEP_RECENT` raw
    /// messages. When that count lands on a result, the exchange is kept whole
    /// — the pre-fix fallback started the tail on the orphaned result.
    #[test]
    fn the_overlap_fallback_keeps_the_exchange_at_its_count_boundary() {
        let mut raw = vec![msg(Role::User, "task")];
        for i in 0..6 {
            raw.push(assistant_call(&format!("c{i}")));
            raw.push(tool_result(&format!("c{i}")));
        }
        raw.push(msg(Role::Assistant, "done"));
        assert_eq!(raw.len() - leveler_context::COMPACT_KEEP_RECENT, 2);
        assert_eq!(raw[2].role, Role::Tool, "the count lands on a result");
        let snap = SnapshotView {
            messages: vec![msg(Role::User, "summary with no overlap")],
            through_ordinal: None,
        };
        let (out, _) = merge_prior_messages(raw.clone(), 0, Some(snap), 0);
        leveler_model::validate_tool_exchange(&out).expect("fallback tail pairs");
        assert_eq!(
            out[1..],
            raw[1..],
            "tail begins on the call, not its result"
        );
    }

    /// A persisted snapshot that itself breaks the tool-exchange invariant
    /// (written by the pre-fix merge) is a derived view that can no longer be
    /// trusted. The merge uses the transcript it was derived from instead —
    /// exactly the no-snapshot path — rather than sending it.
    #[test]
    fn a_snapshot_that_breaks_the_invariant_is_not_used() {
        let raw = vec![
            msg(Role::User, "task"),
            assistant_call("c1"),
            tool_result("c1"),
            msg(Role::Assistant, "done"),
        ];
        let poisoned = SnapshotView {
            messages: vec![msg(Role::User, "summary"), tool_result("c0")],
            through_ordinal: Some(0),
        };
        let (out, _) = merge_prior_messages(raw.clone(), 0, Some(poisoned), 0);
        assert_eq!(out, raw, "the transcript, not the poisoned snapshot");
    }

    /// The same join keeps a result the snapshot already ends with — the
    /// boundary must not drop legal pairs.
    #[test]
    fn a_bounded_load_keeps_a_tool_result_the_snapshot_owns() {
        let raw = vec![tool_result("c1"), msg(Role::Assistant, "after")];
        let snap = SnapshotView {
            messages: vec![assistant_call("c1")],
            through_ordinal: Some(1),
        };
        let (out, _) = merge_prior_messages(raw, 1, Some(snap), 0);
        assert_eq!(out[1].role, Role::Tool, "owned pair must survive: {out:?}");
    }

    fn long_prior(n: usize) -> Vec<Message> {
        let mut v = vec![
            msg(Role::System, "you are leveler"),
            msg(Role::User, "first task: fix login"),
        ];
        for i in 0..n {
            v.push(msg(Role::Assistant, &format!("working step {i} with lots of detail about the codebase path src/auth/login.rs and error handling")));
            v.push(msg(
                Role::User,
                &format!("continue step {i} please keep going on the login timeout issue"),
            ));
        }
        v
    }

    #[test]
    fn budget_prior_under_threshold_prefers_raw_over_stale_snapshot() {
        // Snapshot must never permanently replace later MessageRepository rows.
        let raw = vec![
            msg(Role::User, "first turn"),
            msg(Role::Assistant, "first answer"),
            msg(Role::User, "second turn after snapshot"),
            msg(Role::Assistant, "second answer"),
        ];
        let snap = vec![msg(Role::User, "stale snapshot only")];
        let (out, compacted) = budget_prior_messages(
            raw.clone(),
            Some(SnapshotView {
                messages: snap,
                through_ordinal: None,
            }),
            None,
            None,
            100_000,
        );
        assert!(!compacted);
        assert_eq!(out.len(), raw.len());
        assert!(
            out.iter()
                .any(|m| m.text_content().contains("second turn after snapshot")),
            "under-threshold prior must include post-snapshot raw: {out:?}"
        );
    }

    /// HCH-OPT-5 characterization (reproduction only, no fix in this train):
    /// a legacy snapshot with `through_ordinal: None` whose LAST message is
    /// an in-memory-only injection (scoped rules / nudge — never persisted
    /// to the transcript) defeats the suffix-overlap heuristic, and the
    /// fallback appends the last 12 raw messages on top of a snapshot that
    /// already contains that same recent window.
    #[test]
    fn characterize_overlap_fallback_duplicates_the_recent_window() {
        let raw = long_prior(30);
        // The executor's in-loop snapshot: summary + the recent window it
        // already carries + a memory-only tail (never in the transcript).
        let mut snap = vec![msg(Role::User, "[compact summary of earlier work]")];
        snap.extend_from_slice(&raw[raw.len() - 12..]);
        snap.push(msg(
            Role::System,
            "Project rules:\n- memory-only injection, never persisted",
        ));
        // The duplication is visible on the "merged base already fits"
        // branch (over-threshold raw, under-threshold merge — the common
        // real shape: raw is long, the snapshot is a folded view). When the
        // merge itself is over threshold the subsequent fold swallows the
        // duplicated window into the summarized middle — efficiency waste,
        // not resent duplication.
        let mut expected_base = snap.clone();
        expected_base.extend_from_slice(&raw[raw.len() - 12..]);
        let threshold = leveler_context::estimate_tokens(&expected_base) + 10;
        assert!(leveler_context::estimate_tokens(&raw) > threshold);

        let (out, _) = budget_prior_messages(
            raw.clone(),
            Some(SnapshotView {
                messages: snap,
                through_ordinal: None,
            }),
            None,
            None,
            threshold,
        );

        // Quantify the duplication: every text in the last-12 raw window that
        // appears more than once in the merged output is a duplicate.
        let texts: Vec<String> = out.iter().map(|m| m.text_content()).collect();
        let duplicated = raw[raw.len() - 12..]
            .iter()
            .filter(|m| {
                let t = m.text_content();
                texts.iter().filter(|x| **x == t).count() > 1
            })
            .count();
        let inflation = leveler_context::estimate_tokens(&raw[raw.len() - 12..]);
        let total = leveler_context::estimate_tokens(&out);
        println!(
            "OPT5: duplicated_messages={duplicated} duplicate_window_tokens={inflation} \
             merged_total_tokens={total}"
        );
        assert!(
            duplicated > 0,
            "characterization: the fallback is expected to duplicate the window \
             (if this starts passing with 0, the heuristic changed — re-audit OPT-5)"
        );
    }

    /// HCH-FIX-2: the engine fold must bound the retained recent tail by
    /// TOKENS, not only by message count. A single huge tool result inside
    /// the last 12 messages used to ride through a 24k-threshold fold intact
    /// (keep_recent_tokens = 0), leaving ~5-19x the threshold behind.
    #[test]
    fn engine_fold_bounds_the_retained_tail_by_tokens() {
        let threshold: u64 = 24_000;
        let mut raw = long_prior(20);
        // A huge tool-ish payload well inside the last 12 messages:
        // ~300 KiB ASCII ≈ 75k estimated tokens on its own.
        raw.push(msg(Role::Assistant, &"x".repeat(300 * 1024)));
        for i in 0..3 {
            raw.push(msg(Role::Assistant, &format!("tail {i}")));
        }
        let before = leveler_context::estimate_tokens(&raw);
        assert!(
            before > threshold,
            "precondition: over threshold ({before})"
        );

        let (folded, changed) = budget_prior_messages(
            raw,
            None,
            Some("summary of earlier work"),
            Some("obj"),
            threshold,
        );

        assert!(changed, "an over-threshold prior must fold");
        let after = leveler_context::estimate_tokens(&folded);
        assert!(
            after <= threshold,
            "a {threshold}-token fold must not retain {after} tokens"
        );
    }

    #[test]
    fn budget_prior_merges_snapshot_tail_when_over_threshold() {
        // Oversized raw with a compact snap that ends with a shared suffix;
        // messages after that suffix must appear in the merged prior.
        let mut raw = long_prior(40);
        let shared = msg(Role::Assistant, "shared recent window tail");
        let after = msg(Role::User, "POST_SNAPSHOT_MARKER unique follow-up");
        raw.push(shared.clone());
        raw.push(after.clone());
        let snap = vec![
            msg(Role::User, "[compact summary of early work]"),
            shared.clone(),
        ];
        let tokens = leveler_context::estimate_tokens(&raw);
        assert!(tokens > 200, "need over-threshold raw: {tokens}");
        let (out, compacted) = budget_prior_messages(
            raw,
            Some(SnapshotView {
                messages: snap,
                through_ordinal: None,
            }),
            None,
            Some("fix login"),
            200,
        );
        assert!(compacted);
        let joined: String = out
            .iter()
            .map(|m| m.text_content())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            joined.contains("POST_SNAPSHOT_MARKER")
                || joined.contains("shared recent window")
                || joined.contains("login"),
            "over-threshold merge/compact must not drop the active topic: {joined}"
        );
    }

    #[test]
    fn watermark_merge_survives_duplicate_rounds() {
        // Two textually IDENTICAL user/assistant rounds; the snapshot was
        // taken after the first (message watermark = 2). Suffix-overlap
        // inference matches the snapshot tail against the MOST RECENT
        // occurrence in raw and silently drops one whole round; the explicit
        // watermark appends exactly raw[2..] and keeps both.
        let pad = "padding so the token estimate clears the tiny threshold xxxxxxxxxxxxxxxx";
        let round = [
            msg(Role::User, &format!("run the tests {pad}")),
            msg(Role::Assistant, &format!("all green {pad}")),
        ];
        // A realistically LONG raw transcript whose snapshot is a small
        // folded view: the first identical round sits before the watermark,
        // the second after it. Suffix-overlap inference would match the
        // snapshot tail against the MOST RECENT occurrence and drop a round;
        // the explicit watermark appends exactly raw[wm..].
        let mut raw = long_prior(30);
        raw.extend(round.to_vec());
        let watermark = raw.len() as u64;
        raw.extend(round.to_vec());
        raw.push(msg(
            Role::User,
            &format!("what changed between runs? {pad}"),
        ));
        let snap = vec![
            msg(Role::User, "[compact summary of earlier work]"),
            round[0].clone(),
            round[1].clone(),
        ];
        let threshold = leveler_context::estimate_tokens(&snap)
            + leveler_context::estimate_tokens(&raw[watermark as usize..])
            + 10;
        assert!(
            leveler_context::estimate_tokens(&raw) > threshold,
            "raw must exceed the threshold"
        );

        let (out, _) = budget_prior_messages(
            raw,
            Some(SnapshotView {
                messages: snap,
                through_ordinal: Some(watermark),
            }),
            None,
            None,
            threshold,
        );
        assert_eq!(
            out.len(),
            6,
            "snapshot(3) + raw[wm..](3): the duplicate round after the \
             watermark must survive the merge: {out:?}"
        );
    }

    #[test]
    fn budget_prior_folds_with_the_model_summary_when_given() {
        // The engine pre-request path passes a model handoff briefing; the
        // fold must carry it instead of a bare no-summary breadcrumb.
        let raw = long_prior(40);
        let (out, compacted) = budget_prior_messages(
            raw,
            None,
            Some("HANDOFF_SUMMARY_TEXT for the elided rounds"),
            Some("fix login"),
            200,
        );
        assert!(compacted);
        let joined: String = out
            .iter()
            .map(|m| m.text_content())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            joined.contains("HANDOFF_SUMMARY_TEXT"),
            "the provided summary must survive into the folded transcript: {joined}"
        );
    }

    #[test]
    fn budget_prior_compacts_oversized_history() {
        let raw = long_prior(40);
        let tokens = leveler_context::estimate_tokens(&raw);
        assert!(
            tokens > 100,
            "synthetic history should be non-trivial: {tokens}"
        );
        let (out, compacted) =
            budget_prior_messages(raw.clone(), None, None, Some("fix login"), 200);
        assert!(compacted, "must take compact path when over threshold");
        assert!(
            leveler_context::estimate_tokens(&out) < tokens || out.len() < raw.len(),
            "compacted transcript should shrink"
        );
    }
}
