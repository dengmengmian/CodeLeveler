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

use tokio_util::sync::CancellationToken;

use leveler_core::{SessionId, TaskId, TurnId};
use leveler_lifecycle::{AgentState, SessionStatus, StopReason, VerificationStatus};
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
    match merge_prior_messages(raw, snapshot, threshold) {
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
    snapshot: Option<SnapshotView>,
    threshold: u64,
) -> (Vec<leveler_model::Message>, PriorMerge) {
    let raw_tokens = leveler_context::estimate_tokens(&raw);
    if raw_tokens <= threshold {
        return (raw, PriorMerge::Fits { merged: false });
    }

    let base = match snapshot {
        Some(view) if !view.messages.is_empty() => match view.through_ordinal {
            Some(n) if (n as usize) <= raw.len() => {
                // Exact watermark: everything after the first `n` transcript
                // messages post-dates the snapshot. No inference, so rounds
                // that repeat earlier text verbatim are never mistaken for
                // the snapshot's own tail and dropped.
                let mut out = view.messages;
                out.extend_from_slice(&raw[n as usize..]);
                out
            }
            Some(n) => {
                // A watermark beyond the live transcript means the transcript
                // was truncated after the snapshot (context ops normally
                // rewrite the snapshot too). Never guess a slice: fall back
                // to the legacy overlap merge and say so.
                tracing::warn!(
                    through_ordinal = n,
                    raw_len = raw.len(),
                    "context snapshot watermark exceeds transcript; using overlap merge"
                );
                merge_snapshot_with_raw_tail(view.messages, &raw)
            }
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
    // No overlap (pure summary snapshot): keep snap + trailing raw window.
    let keep = leveler_context::COMPACT_KEEP_RECENT.min(raw.len());
    let mut out = snap;
    out.extend_from_slice(&raw[raw.len() - keep..]);
    out
}

fn messages_slice_eq(a: &[leveler_model::Message], b: &[leveler_model::Message]) -> bool {
    a.len() == b.len() && a.iter().zip(b.iter()).all(|(x, y)| x == y)
}

/// The persistent task engine.
///
/// Persistence enters exclusively through [`EngineStores`] — narrow
/// capability ports the composition root wires to its adapter (SQLite
/// locally). The engine never names a concrete database.
/// Ask for a handoff briefing only when the raw history is over the fold
/// threshold. Who writes it — and whether that costs a model call — is the
/// caller's business; a `None` briefing degrades to the bare-breadcrumb fold
/// and never blocks the turn.
async fn summarize_if_over(
    summarizer: &dyn crate::ContextSummarizer,
    raw: &[leveler_model::Message],
) -> Option<String> {
    if leveler_context::estimate_tokens(raw) <= leveler_context::PRE_REQUEST_COMPACT_THRESHOLD {
        return None;
    }
    summarizer.summarize(raw).await
}

pub struct TaskEngine {
    pub stores: EngineStores,
    /// This runtime's durable identity (from the composition root). Task
    /// ownership is acquired for it at every execution entry.
    pub runtime_id: leveler_core::RuntimeId,
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
}

impl TaskEngine {
    /// Commit the canonical terminal event and every session lifecycle column
    /// (outcome + status + state) atomically, then forward the event. The
    /// engine is the ONE writer of the session lifecycle — no app layer stamps
    /// a second copy — and an observer can never see an uncommitted fact.
    pub async fn finish_task(
        &self,
        token: &leveler_core::OwnershipToken,
        session_id: &SessionId,
        outcome: TaskOutcome,
        verification: VerificationStatus,
        reason: Option<String>,
        stop: Option<StopReason>,
        status: SessionStatus,
        state: AgentState,
        observer: &mut (dyn FnMut(EngineEvent) + Send),
    ) -> Result<(), EngineError> {
        let event = EngineEvent::TaskFinished {
            outcome,
            verification,
            reason,
            stop,
        };
        let (event_type, payload) = event.to_row()?;
        self.stores
            .terminal
            .finish_task_owned(
                token,
                session_id,
                &event_type,
                &payload,
                outcome,
                verification,
                status,
                state,
                leveler_core::now(),
            )
            .await?;
        observer(event);
        Ok(())
    }

    /// Mark the session running before the first turn. The engine owns this
    /// transition too — clients observe lifecycle, they never write it.
    ///
    /// Also the ONE seam where the durable task identity is guaranteed: every
    /// execution entry (run/chat/resume) passes here, so a session created by
    /// any path — including one that predates the tasks table — has its task
    /// row before the first turn. Returns that task id.
    /// Acquire (or same-runtime reacquire) ownership of the session's task.
    /// A task owned by a DIFFERENT runtime is a hard conflict - never
    /// auto-stolen. The epoch always advances, fencing prior incarnations.
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
        if let Some(owner) = &current.runtime
            && owner != &self.runtime_id
        {
            return Err(EngineError::OwnershipConflict {
                task_id,
                owner: owner.clone(),
                epoch: current.epoch,
                this_runtime: self.runtime_id.clone(),
            });
        }
        Ok(self
            .stores
            .ownership
            .acquire(&task_id, &self.runtime_id, current.epoch)
            .await?)
    }

    /// Mark the session running before the first turn (fenced), acquiring
    /// ownership first — the ONE seam every execution entry passes through.
    pub async fn mark_running(
        &self,
        session_id: &SessionId,
    ) -> Result<leveler_core::OwnershipToken, EngineError> {
        let token = self.acquire_ownership(session_id).await?;
        self.stores
            .sessions
            .update_status_owned(
                &token,
                session_id,
                SessionStatus::Running,
                AgentState::Execute,
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
        let record = SessionRecord::new(
            session.workspace.clone(),
            session.goal.clone(),
            session.model.clone(),
            leveler_core::now(),
        );
        self.stores.sessions.create(&record).await?;
        let id = SessionId::new(record.id);
        self.stores
            .sessions
            .set_execution(
                &id,
                &session.mode,
                session.sandbox,
                session.kind.as_str(),
                leveler_core::now(),
            )
            .await?;
        self.stores
            .tasks
            .ensure_for_session(&id, leveler_core::now())
            .await?;
        Ok(id)
    }

    /// Long-goal P3: the checkpoint-backed pre-request fold.
    ///
    /// Over the fold threshold, prefer a FRESH durable checkpoint (one whose
    /// delta still fits the threshold); when the newest checkpoint is stale
    /// or absent, cut a `ContextCompaction` checkpoint at the current
    /// committed boundary and continue from its block plus a bounded recent
    /// tail. `None` = keep the pre-checkpoint path: transcript under the
    /// threshold, no goal in scope, or checkpoint creation failed — every
    /// fold leaves the durable transcript untouched, so the fallback
    /// degrades only to exactly the pre-P3 context, never to lost history.
    /// The model-visible prior context for a turn: the ONE assembly every
    /// path shares.
    ///
    /// A checkpoint's block wins when one is fresh enough; otherwise the
    /// transcript is merged with the latest snapshot and folded if it still
    /// does not fit. No caller assembles its own, and none hands a model the
    /// raw transcript — an unassembled history is unbounded by construction,
    /// and on a long session that is the whole history resent every turn.
    pub async fn assembled_prior(
        &self,
        log: &EventLog<'_>,
        session_id: &SessionId,
        raw: crate::RawTranscript,
        objective: Option<&str>,
        workspace: Option<&dyn crate::ports::WorkspaceFacts>,
        summarizer: &dyn crate::ContextSummarizer,
        cancellation: &CancellationToken,
        observer: &mut (dyn FnMut(EngineEvent) + Send),
    ) -> Result<Vec<leveler_model::Message>, EngineError> {
        if let Some(prior) = self
            .checkpointed_prior(
                log,
                session_id,
                &raw,
                workspace,
                summarizer,
                cancellation,
                observer,
            )
            .await?
        {
            return Ok(prior);
        }
        let context = raw
            .assemble(
                log,
                Some(summarizer),
                objective,
                leveler_context::PRE_REQUEST_COMPACT_THRESHOLD,
            )
            .await?;
        if context.compacted {
            log.append(None, context.snapshot_event(), observer).await?;
        }
        Ok(context.prior)
    }

    /// Load the transcript a request needs, reading only the tail when both
    /// watermarks prove the earlier rows unreachable. Falls back to the full
    /// load whenever either is unknown; see
    /// [`crate::session_context::RawTranscript::load_bounded`].
    pub async fn load_request_transcript(
        &self,
        session_id: &SessionId,
        strict: Option<&str>,
    ) -> Result<crate::RawTranscript, EngineError> {
        let threshold = leveler_context::PRE_REQUEST_COMPACT_THRESHOLD;
        let snapshot_ordinal = EventLog::new(self.stores.events.as_ref(), session_id.clone())
            .latest_context_snapshot(None)
            .await?
            .and_then(|view| view.through_ordinal);
        let checkpoint_ordinal =
            crate::checkpoint::checkpoint_transcript_ordinal(&self.stores, session_id).await?;
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

    pub async fn checkpointed_prior(
        &self,
        log: &EventLog<'_>,
        session_id: &SessionId,
        raw: &crate::RawTranscript,
        workspace: Option<&dyn crate::ports::WorkspaceFacts>,
        summarizer: &dyn crate::ContextSummarizer,
        cancellation: &CancellationToken,
        observer: &mut (dyn FnMut(EngineEvent) + Send),
    ) -> Result<Option<Vec<leveler_model::Message>>, EngineError> {
        let _ = cancellation;
        let threshold = leveler_context::PRE_REQUEST_COMPACT_THRESHOLD;
        if leveler_context::estimate_tokens(&raw.messages) <= threshold {
            return Ok(None);
        }
        // Cheap goal probe BEFORE any model call: a session with no goal
        // keeps the pre-checkpoint path bit-for-bit (including exactly one
        // summarization call, which mocks and cost accounting rely on).
        let Some(task) = self.stores.tasks.task_for_session(session_id).await? else {
            return Ok(None);
        };
        if self.stores.goals.for_task(&task).await?.is_empty() {
            return Ok(None);
        }
        if let Some(prior) =
            crate::checkpoint::resume_prior_from_checkpoint(&self.stores, session_id, raw).await?
            && leveler_context::estimate_tokens(&prior) <= threshold
        {
            return Ok(Some(prior));
        }
        let summary = summarize_if_over(summarizer, &raw.messages).await;
        match crate::checkpoint::create_goal_checkpoint(
            &self.stores,
            session_id,
            leveler_lifecycle::CheckpointReason::ContextCompaction,
            workspace,
            crate::checkpoint::SemanticRecap::briefing(summary.as_deref()),
        )
        .await
        {
            Ok(Some(record)) => {
                let event = crate::checkpoint::checkpoint_created_event(&record);
                log.append(None, event, observer).await?;
                // The checkpoint block, plus a bounded raw tail for local
                // continuity — the same recency window the pre-P3 fold kept.
                let tail_start = raw
                    .messages
                    .len()
                    .saturating_sub(leveler_context::COMPACT_KEEP_RECENT);
                let mut prior = Vec::with_capacity(1 + raw.messages.len() - tail_start);
                prior.push(leveler_model::Message {
                    role: leveler_model::Role::User,
                    content: vec![leveler_model::ContentPart::Text {
                        text: record.payload.context_block(),
                    }],
                });
                prior.extend_from_slice(&raw.messages[tail_start..]);
                Ok(Some(prior))
            }
            Ok(None) => Ok(None),
            Err(error) => {
                tracing::warn!(
                    %error,
                    "context-compaction checkpoint failed; using the pre-checkpoint fold"
                );
                Ok(None)
            }
        }
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
                call_id: call.call_id.clone(),
                name: call.name.clone(),
                is_error: true,
                preview: "user-acknowledged crash recovery: the interrupted call's outcome \
                          is unknown; the workspace was verified manually and the call was \
                          not replayed"
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
    use leveler_model::{Message, Role};

    fn msg(role: Role, text: &str) -> Message {
        Message::text(role, text)
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

    #[test]
    fn cumulative_rounds_do_not_reset_on_continue_merge() {
        // Mirrors continue_active_goal: epoch totals grow, not reset.
        let mut progress = leveler_lifecycle::ProgressLedger::default();
        progress.accumulate_drive_rounds(5);
        progress.accumulate_drive_rounds(3);
        assert_eq!(progress.cumulative_rounds, 8);
        // A fresh Content turn with terminal progress must not seed (epoch gate).
        progress.enter_terminal();
        assert!(progress.is_terminal_for_inheritance());
        assert!(!crate::turn::should_seed_task_state(
            None,
            Some(&progress),
            false
        ));
    }
}
