//! The ONE recovery/context-loading entry (convergence plan phase 3).
//!
//! Every consumer of a session's model-visible context — chat turns, resume,
//! goal continuation, goal history injection, and the app's side-question
//! path — loads through [`RawTranscript`] and assembles through
//! [`RawTranscript::assemble`]. That keeps exactly one implementation of:
//! transcript parsing (strict vs lossy), snapshot lookup, watermark-based
//! merging, and the compaction fold. A caller that assembled a compacted
//! context persists it via [`SessionContext::snapshot_event`], which stamps
//! the transcript watermark so the next restore appends exact tails instead
//! of inferring overlap.

use leveler_context::FoldRequirement;
use leveler_core::SessionId;
use leveler_storage::MessageStore;

use crate::engine::{PriorMerge, merge_prior_messages_measured};
use crate::log::EventLog;
use crate::{EngineError, EngineEvent};

/// Produces the model handoff briefing for messages about to be folded away.
/// [`RawTranscript::assemble`] asks for one only after merging the latest
/// snapshot with its tail still leaves the context over threshold — so a
/// turn that fits from the snapshot never pays for a summary it would drop.
///
/// # The elision facts are the caller's
///
/// `keep_recent` / `keep_recent_tokens` ARE the fold's retention, handed to
/// the summarizer so it replaces exactly the rounds the fold is about to
/// remove. The summarizer must not re-derive a retention of its own: a
/// briefing written from a wider tail leaves the messages between the two
/// cuts in neither the request nor the briefing — history that vanishes with
/// no record anywhere in the prompt. One decision, one owner.
///
/// # Failure contract
///
/// The summarizer owns ONLY the summary call. It does not decide what a
/// failure costs: that is [`leveler_context::FoldRequirement`]'s job, applied
/// by the context lifecycle.
///
/// * `Ok(Some(briefing))` — a briefing was produced.
/// * `Ok(None)` — no briefing is available (provider fault, summary deadline,
///   no auxiliary budget, rejected output, or nothing worth summarizing).
///   The lifecycle turns this into "keep the original history" under soft
///   pressure and "mechanical fold + capacity check" under hard pressure.
/// * `Err(EngineError::Cancelled)` — the TASK was cancelled. This is not a
///   summary failure and must propagate.
/// * any other `Err` — a fault a fold cannot repair (ownership, persistence,
///   configuration). It must not be hidden behind a soft continue.
#[async_trait::async_trait]
pub trait ContextSummarizer: Send + Sync {
    async fn summarize(
        &self,
        messages: &[leveler_model::Message],
        keep_recent: usize,
        keep_recent_tokens: u64,
    ) -> Result<Option<String>, EngineError>;
}

/// The parsed transcript of a session, in ordinal order.
///
/// `offset` is the ordinal `messages[0]` sits at. It is `0` for a full load —
/// the general case — and non-zero only for a load that proved the earlier
/// rows unreachable (see [`transcript_start`]). Every consumer that indexes
/// by ordinal has to subtract it, which is why it travels with the messages
/// instead of being remembered by the caller.
pub struct RawTranscript {
    pub messages: Vec<leveler_model::Message>,
    offset: u64,
}

impl RawTranscript {
    /// The ordinal `messages[0]` sits at.
    pub fn offset(&self) -> u64 {
        self.offset
    }

    /// Total transcript length in ordinals, including rows not loaded.
    pub fn stored_len(&self) -> u64 {
        self.offset + self.messages.len() as u64
    }

    /// The loaded messages from stored ordinal `ordinal` onward, or `None`
    /// when that ordinal is before this load began or past its end. A caller
    /// that needs a slice this transcript cannot serve must fall back rather
    /// than silently take a shorter history.
    pub fn slice_from_ordinal(&self, ordinal: u64) -> Option<&[leveler_model::Message]> {
        let local = ordinal.checked_sub(self.offset)? as usize;
        (local <= self.messages.len()).then(|| &self.messages[local..])
    }
}

/// An assembled model-visible context: what the next request should see, and
/// the watermark a new snapshot of it must carry.
pub struct SessionContext {
    /// Messages for the next model request.
    pub prior: Vec<leveler_model::Message>,
    /// The context was folded/merged; the caller should persist
    /// [`SessionContext::snapshot_event`] so the next load starts shorter.
    pub compacted: bool,
    /// Transcript watermark at assembly time (`raw.len()`).
    through_ordinal: u64,
}

impl RawTranscript {
    /// Strict load: any unparsable row is a hard `Corrupt` error naming
    /// `what` (resume and continuation must reconstruct exactly).
    pub async fn load_strict(
        messages: &dyn MessageStore,
        session_id: &SessionId,
        what: &str,
    ) -> Result<Self, EngineError> {
        let payloads = messages.load(session_id).await?;
        let messages = payloads
            .iter()
            .map(|p| serde_json::from_str(p))
            .collect::<Result<Vec<leveler_model::Message>, _>>()
            .map_err(|error| EngineError::Corrupt(format!("unreplayable {what}: {error}")))?;
        Ok(Self {
            messages,
            offset: 0,
        })
    }

    /// Lossy load: an unreadable legacy row only loses context (interactive
    /// chat and side questions tolerate that; resume must not).
    pub async fn load_lossy(
        messages: &dyn MessageStore,
        session_id: &SessionId,
    ) -> Result<Self, EngineError> {
        let payloads = messages.load(session_id).await?;
        let messages = payloads
            .iter()
            .filter_map(|p| serde_json::from_str(p).ok())
            .collect();
        Ok(Self {
            messages,
            offset: 0,
        })
    }

    /// Load only what the next request can reach.
    ///
    /// Same result as [`Self::load_lossy`] for every consumer, reached with
    /// less work when the transcript is provably over `threshold` and a
    /// watermark says the earlier rows are unreachable. `checkpoint_ordinal`
    /// is the goal checkpoint's transcript watermark when one exists — it
    /// binds too, because resume splices the transcript from there.
    ///
    /// Falls back to the full load whenever anything is unknown. A shorter
    /// history than a consumer asked for would be a silent wrong answer; the
    /// work saved is never worth risking that.
    /// `strict` names what an unreadable row means, exactly as it does for the
    /// two full loaders: a hard `Corrupt` error for a path that must
    /// reconstruct precisely, or a tolerated loss of context. Strictness
    /// applies to the rows this load actually reads. Rows before the watermark
    /// are not skipped silently — they are the ones the snapshot or checkpoint
    /// already represents, and neither is reachable by the next request.
    pub async fn load_bounded(
        messages: &dyn MessageStore,
        session_id: &SessionId,
        threshold: u64,
        snapshot_ordinal: Option<u64>,
        checkpoint_ordinal: Option<u64>,
        strict: Option<&str>,
    ) -> Result<Self, EngineError> {
        let stored_bytes = messages.total_bytes(session_id).await?;
        let start = transcript_start(
            stored_bytes,
            threshold,
            snapshot_ordinal,
            checkpoint_ordinal,
        );
        let TranscriptStart::Ordinal(offset) = start else {
            return match strict {
                Some(what) => Self::load_strict(messages, session_id, what).await,
                None => Self::load_lossy(messages, session_id).await,
            };
        };
        let payloads = messages.load_from(session_id, offset).await?;
        let messages = match strict {
            Some(what) => payloads
                .iter()
                .map(|p| serde_json::from_str(p))
                .collect::<Result<Vec<leveler_model::Message>, _>>()
                .map_err(|error| EngineError::Corrupt(format!("unreplayable {what}: {error}")))?,
            None => payloads
                .iter()
                .filter_map(|p| serde_json::from_str(p).ok())
                .collect(),
        };
        Ok(Self { messages, offset })
    }

    pub fn is_empty(&self) -> bool {
        self.messages.is_empty()
    }

    /// Assemble the model-visible context: latest snapshot (with watermark)
    /// plus the post-snapshot tail, folded under `threshold` if needed.
    /// `summarizer` is consulted for a handoff briefing only when that fold
    /// is actually needed; `None` folds with a bare breadcrumb.
    ///
    /// This convenience carries no hard capacity, so a fold here is always
    /// quality pressure. A caller that knows the model's resolved hard bound
    /// must use [`Self::assemble_measured`] so the two bounds stay apart.
    pub async fn assemble(
        self,
        log: &EventLog<'_>,
        summarizer: Option<&dyn ContextSummarizer>,
        active_objective: Option<&str>,
        threshold: u64,
    ) -> Result<SessionContext, EngineError> {
        self.assemble_measured(
            log,
            summarizer,
            active_objective,
            threshold,
            None,
            leveler_context::COMPACT_KEEP_RECENT,
            threshold / 2,
            &leveler_context::estimate_tokens,
        )
        .await
    }

    /// Merge the stored snapshot and tail under the caller's actual request
    /// pressure, without discarding any further context. Side conversations
    /// append their own history before deciding whether to summarize.
    pub async fn assemble_unfolded_measured(
        self,
        log: &EventLog<'_>,
        threshold: u64,
        measure: &(dyn Fn(&[leveler_model::Message]) -> u64 + Send + Sync),
    ) -> Result<SessionContext, EngineError> {
        let through_ordinal = self.stored_len();
        let snapshot = log.latest_context_snapshot(None).await?;
        let (prior, _) =
            merge_prior_messages_measured(self.messages, self.offset, snapshot, threshold, measure);
        Ok(SessionContext {
            prior,
            compacted: false,
            through_ordinal,
        })
    }

    #[allow(clippy::too_many_arguments)]
    pub async fn assemble_measured(
        self,
        log: &EventLog<'_>,
        summarizer: Option<&dyn ContextSummarizer>,
        active_objective: Option<&str>,
        quality_threshold: u64,
        hard_capacity: Option<u64>,
        keep_recent: usize,
        keep_recent_tokens: u64,
        measure: &(dyn Fn(&[leveler_model::Message]) -> u64 + Send + Sync),
    ) -> Result<SessionContext, EngineError> {
        // The watermark is an ABSOLUTE transcript ordinal: a bounded load may
        // start at a non-zero `offset`, and the next load indexes the snapshot
        // watermark against its own rows.
        let through_ordinal = self.stored_len();
        let raw_offset = self.offset;
        let snapshot = log.latest_context_snapshot(None).await?;
        let (prior, compacted) = match merge_prior_messages_measured(
            self.messages,
            raw_offset,
            snapshot,
            quality_threshold,
            measure,
        ) {
            (base, PriorMerge::Fits { merged }) => (base, merged),
            (base, PriorMerge::Over { base_tokens }) => {
                // ONE classification, shared with the coding drive: the measured
                // request's pressure decides what a failed briefing costs.
                let requirement =
                    FoldRequirement::classify(base_tokens, quality_threshold, hard_capacity);
                let summary = match summarizer {
                    Some(summarizer) => {
                        summarizer
                            .summarize(&base, keep_recent, keep_recent_tokens)
                            .await?
                    }
                    None => None,
                };
                match requirement {
                    FoldRequirement::HardRequired => {
                        // The briefing may be absent (provider fault, deadline,
                        // no budget). The request cannot legally be sent
                        // unfolded, so fold MECHANICALLY, then prove the result
                        // is sendable BEFORE it is committed.
                        let folded = leveler_context::compact_messages(
                            &base,
                            keep_recent,
                            keep_recent_tokens,
                            summary.as_deref(),
                            active_objective,
                        );
                        let projected = measure(&folded);
                        if let Some(capacity) = hard_capacity
                            && projected > capacity
                        {
                            return Err(EngineError::ContextManagementFailure(format!(
                                "compaction could not fit the request into the model's hard context \
                                 capacity: projected {projected} tokens, capacity {capacity}"
                            )));
                        }
                        let changed = folded != base;
                        (folded, changed)
                    }
                    // Soft or None: sending the uncompacted history is still
                    // legal. A briefing nobody could produce must not cost the
                    // turn, so keep the original active history.
                    _ => {
                        if summarizer.is_some() && summary.is_none() {
                            (base, false)
                        } else {
                            let folded = leveler_context::compact_messages(
                                &base,
                                keep_recent,
                                keep_recent_tokens,
                                summary.as_deref(),
                                active_objective,
                            );
                            let changed = folded != base;
                            (folded, changed)
                        }
                    }
                }
            }
        };
        Ok(SessionContext {
            prior,
            compacted,
            through_ordinal,
        })
    }
}

impl SessionContext {
    /// The snapshot event a caller persists when `compacted` is set: the
    /// assembled context, watermarked at the transcript length it supersedes.
    pub fn snapshot_event(&self) -> EngineEvent {
        EngineEvent::ContextSnapshot {
            messages: self.prior.clone(),
            through_ordinal: Some(self.through_ordinal),
        }
    }
}

/// How much of a session's transcript the next request can possibly need.
///
/// The full transcript is the default answer and the only safe one in
/// general: a snapshot is never a permanent replacement for later turns, so
/// a transcript that still fits the fold threshold is sent whole. Skipping
/// the rest of it is allowed only when every consumer's watermark says the
/// earlier rows cannot be reached.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TranscriptStart {
    /// Load everything.
    Beginning,
    /// Load from this ordinal on; rows before it are provably unreachable.
    Ordinal(u64),
}

/// Decide where a load may start.
///
/// `stored_bytes` gates the whole optimization: the token estimator charges at
/// least one token per four bytes for every kind of content, so `bytes / 4`
/// under the threshold means the transcript *might* still fit whole, and a
/// transcript that fits is sent whole. Only when it provably does not fit is
/// the merge path certain, and only then does a watermark bound the load.
///
/// Both watermarks bind, and the earlier one wins. The snapshot's says where
/// the merged tail begins; a goal checkpoint's says where ITS tail begins, and
/// resume splices the transcript from there. Loading from the later of the two
/// would hand the checkpoint path a transcript that starts after the point it
/// needs to splice from — a silently shorter history, which is worse than the
/// work this saves.
pub(crate) fn transcript_start(
    stored_bytes: u64,
    threshold: u64,
    snapshot_ordinal: Option<u64>,
    checkpoint_ordinal: Option<u64>,
) -> TranscriptStart {
    // Lower bound on what the estimator would charge. Equal is not over.
    if stored_bytes / 4 <= threshold {
        return TranscriptStart::Beginning;
    }
    // Without a watermarked snapshot the merge falls back to inferring the
    // overlap from the transcript itself, which needs all of it.
    let Some(snapshot) = snapshot_ordinal else {
        return TranscriptStart::Beginning;
    };
    let start = match checkpoint_ordinal {
        Some(checkpoint) => snapshot.min(checkpoint),
        None => snapshot,
    };
    if start == 0 {
        return TranscriptStart::Beginning;
    }
    TranscriptStart::Ordinal(start)
}

#[cfg(test)]
mod transcript_start_tests {
    use super::{TranscriptStart, transcript_start};

    /// A transcript that might still fit is loaded whole: under the threshold
    /// the snapshot is not consulted at all, because a snapshot never
    /// permanently replaces the transcript for a later turn.
    #[test]
    fn a_transcript_that_may_still_fit_is_loaded_whole() {
        assert_eq!(
            transcript_start(4_000, 24_000, Some(500), None),
            TranscriptStart::Beginning
        );
        // Exactly at the bound is not over it.
        assert_eq!(
            transcript_start(96_000, 24_000, Some(500), None),
            TranscriptStart::Beginning
        );
    }

    /// Provably over the threshold, with a watermark: the rows before it are
    /// unreachable and are not read.
    #[test]
    fn a_watermark_bounds_a_transcript_that_cannot_fit() {
        assert_eq!(
            transcript_start(96_004, 24_000, Some(500), None),
            TranscriptStart::Ordinal(500)
        );
    }

    /// The earlier watermark wins. A checkpoint splices the transcript from
    /// its own ordinal, so starting after that would hand it a shorter
    /// history than it asks for — the failure this test exists to prevent.
    #[test]
    fn the_earlier_of_the_two_watermarks_wins() {
        assert_eq!(
            transcript_start(1_000_000, 24_000, Some(900), Some(300)),
            TranscriptStart::Ordinal(300)
        );
        assert_eq!(
            transcript_start(1_000_000, 24_000, Some(300), Some(900)),
            TranscriptStart::Ordinal(300)
        );
    }

    /// A legacy snapshot carries no watermark, so the merge infers the overlap
    /// from the transcript and needs all of it.
    #[test]
    fn no_watermark_means_no_bound() {
        assert_eq!(
            transcript_start(1_000_000, 24_000, None, Some(300)),
            TranscriptStart::Beginning
        );
    }

    /// A watermark of zero bounds nothing; say so rather than "load from 0",
    /// so the caller has one shape for "read it all".
    #[test]
    fn a_zero_watermark_is_the_beginning() {
        assert_eq!(
            transcript_start(1_000_000, 24_000, Some(0), None),
            TranscriptStart::Beginning
        );
        assert_eq!(
            transcript_start(1_000_000, 24_000, Some(500), Some(0)),
            TranscriptStart::Beginning
        );
    }
}

#[cfg(test)]
mod projection_pressure_tests {
    use super::*;
    use leveler_model::{
        ContentPart, Message, ReasoningReplayContract, ReasoningRetention, RequestProjection, Role,
    };

    #[tokio::test]
    async fn raw_reasoning_that_is_not_replayed_does_not_fold_history() {
        let mut messages = vec![Message::text(Role::User, "the objective")];
        for _ in 0..20 {
            messages.push(Message {
                origin: None,
                role: Role::Assistant,
                content: vec![
                    ContentPart::Reasoning {
                        text: "r".repeat(10_000),
                    },
                    ContentPart::Text {
                        text: "observed".into(),
                    },
                ],
            });
        }
        assert!(leveler_context::estimate_tokens(&messages) > 1_000);
        let original = messages.clone();
        let raw = RawTranscript {
            messages,
            offset: 0,
        };
        let store = leveler_storage::MemoryEventStore::default();
        let log = EventLog::new(&store, SessionId::generate());
        let measure = |messages: &[Message]| {
            RequestProjection::project(
                messages,
                &[],
                ReasoningReplayContract::NONE,
                ReasoningRetention::All,
            )
            .estimated_tokens()
        };
        let context = raw
            .assemble_measured(&log, None, None, 1_000, None, 12, 500, &measure)
            .await
            .unwrap();
        assert!(!context.compacted);
        assert_eq!(context.prior, original);
    }
}

#[cfg(test)]
mod fold_semantics_tests {
    //! PR5e — the ONE fold-failure contract every active-context entry shares.
    //!
    //! These drive the real `assemble_measured` over a synthetic transcript.
    //! The pressure figure is the message COUNT, not the estimator, so each
    //! test chooses the regime (soft / hard) instead of discovering it; the
    //! fold itself is the production `compact_messages`.

    use super::*;
    use leveler_model::{COMPACTION_BREADCRUMB_MARKER, Message, Role};

    /// What the summarizer does when asked for a briefing.
    enum Briefing {
        Produced(&'static str),
        /// No briefing is available (provider fault, deadline, no budget).
        Unavailable,
        /// The task was cancelled: not a summary failure.
        Cancelled,
        /// A fault a fold cannot repair (ownership, persistence, config).
        Fatal,
    }

    struct FakeSummarizer(Briefing);

    #[async_trait::async_trait]
    impl ContextSummarizer for FakeSummarizer {
        async fn summarize(
            &self,
            _messages: &[Message],
            _keep_recent: usize,
            _keep_recent_tokens: u64,
        ) -> Result<Option<String>, EngineError> {
            match self.0 {
                Briefing::Produced(text) => Ok(Some(text.to_string())),
                Briefing::Unavailable => Ok(None),
                Briefing::Cancelled => Err(EngineError::Cancelled),
                Briefing::Fatal => Err(EngineError::Config("accounting store is down".into())),
            }
        }
    }

    /// A transcript with a foldable middle: system + objective head, twenty
    /// exchanges, and a tail. Neither the head nor the tail is empty.
    fn transcript() -> Vec<Message> {
        let mut messages = vec![
            Message::text(Role::System, "you are a coding agent"),
            Message::text(Role::User, "the objective"),
        ];
        for index in 0..20 {
            messages.push(Message::text(Role::User, format!("user turn {index}")));
            messages.push(Message::text(
                Role::Assistant,
                format!("assistant turn {index}"),
            ));
        }
        messages
    }

    fn count(messages: &[Message]) -> u64 {
        messages.len() as u64
    }

    async fn assemble(
        log: &EventLog<'_>,
        summarizer: Option<&dyn ContextSummarizer>,
        quality_threshold: u64,
        hard_capacity: Option<u64>,
    ) -> (Result<SessionContext, EngineError>, Vec<Message>) {
        let original = transcript();
        let raw = RawTranscript {
            messages: original.clone(),
            offset: 0,
        };
        let result = raw
            .assemble_measured(
                log,
                summarizer,
                None,
                quality_threshold,
                hard_capacity,
                2,
                0,
                &count,
            )
            .await;
        (result, original)
    }

    /// Soft pressure + no briefing: the uncompacted history is still legal, so
    /// the session continues with the ORIGINAL messages and no committed fold.
    #[tokio::test]
    async fn soft_pressure_keeps_the_original_history_when_no_briefing_arrives() {
        let store = leveler_storage::MemoryEventStore::default();
        let log = EventLog::new(&store, SessionId::generate());
        let summarizer = FakeSummarizer(Briefing::Unavailable);
        let (result, original) = assemble(&log, Some(&summarizer), 5, Some(1_000)).await;
        let context = result.expect("a soft failure must not abort assembly");
        assert!(!context.compacted, "nothing may be committed");
        assert_eq!(
            context.prior, original,
            "the original active history stands"
        );
    }

    /// Soft pressure + a briefing: the fold still happens, as it always did.
    #[tokio::test]
    async fn soft_pressure_folds_when_a_briefing_is_produced() {
        let store = leveler_storage::MemoryEventStore::default();
        let log = EventLog::new(&store, SessionId::generate());
        let summarizer = FakeSummarizer(Briefing::Produced("read src/lib.rs"));
        let (result, _) = assemble(&log, Some(&summarizer), 5, Some(1_000)).await;
        let context = result.expect("assembly");
        assert!(context.compacted, "a produced briefing folds");
        assert!(
            context
                .prior
                .iter()
                .any(|message| message.text_content().contains("read src/lib.rs"))
        );
    }

    /// A cancelled task is NOT a soft summary failure: it propagates so the
    /// caller records an interrupted turn instead of issuing a model request.
    #[tokio::test]
    async fn cancellation_is_never_swallowed_as_a_soft_summary_failure() {
        let store = leveler_storage::MemoryEventStore::default();
        let log = EventLog::new(&store, SessionId::generate());
        let summarizer = FakeSummarizer(Briefing::Cancelled);
        let (result, _) = assemble(&log, Some(&summarizer), 5, Some(1_000)).await;
        assert!(
            matches!(result, Err(EngineError::Cancelled)),
            "cancellation must propagate, not become a continue"
        );
    }

    /// An infrastructure fault is not a briefing that "could not be produced":
    /// hiding it behind a soft continue would run on broken accounting.
    #[tokio::test]
    async fn a_fatal_summarizer_error_is_not_hidden_as_soft_pressure() {
        let store = leveler_storage::MemoryEventStore::default();
        let log = EventLog::new(&store, SessionId::generate());
        let summarizer = FakeSummarizer(Briefing::Fatal);
        let (result, _) = assemble(&log, Some(&summarizer), 5, Some(1_000)).await;
        assert!(
            matches!(result, Err(EngineError::Config(_))),
            "a persistence/config fault must stay fatal"
        );
    }

    /// Hard pressure + no briefing: the request cannot be sent unfolded, so the
    /// fold happens MECHANICALLY and is then proven legal.
    #[tokio::test]
    async fn hard_pressure_folds_mechanically_when_no_briefing_arrives() {
        let store = leveler_storage::MemoryEventStore::default();
        let log = EventLog::new(&store, SessionId::generate());
        // 41 messages over a 20-token capacity: no fold, no legal request.
        let summarizer = FakeSummarizer(Briefing::Unavailable);
        let (result, _) = assemble(&log, Some(&summarizer), 5, Some(20)).await;
        let context = result.expect("the mechanical fold fits");
        assert!(context.compacted);
        assert!(
            context.prior.iter().any(|message| message
                .text_content()
                .contains(COMPACTION_BREADCRUMB_MARKER)),
            "the mechanical fallback names the fold with a breadcrumb"
        );
        assert!(
            count(&context.prior) <= 20,
            "the folded request is within hard capacity"
        );
    }

    /// Hard pressure + no briefing + a fold that still cannot fit: the turn
    /// fails explicitly and nothing is committed.
    #[tokio::test]
    async fn hard_pressure_fails_explicitly_when_the_fold_still_cannot_fit() {
        let store = leveler_storage::MemoryEventStore::default();
        let log = EventLog::new(&store, SessionId::generate());
        let summarizer = FakeSummarizer(Briefing::Unavailable);
        let (result, _) = assemble(&log, Some(&summarizer), 5, Some(3)).await;
        assert!(
            matches!(result, Err(EngineError::ContextManagementFailure(_))),
            "an impossible context is named, not silently sent"
        );
        assert!(
            log.latest_context_snapshot(None).await.unwrap().is_none(),
            "a refused fold must not leave a partial ContextSnapshot"
        );
    }

    /// Hard pressure + a briefing: the fold carries the briefing and the
    /// capacity check still gates it.
    #[tokio::test]
    async fn hard_pressure_folds_with_a_briefing_when_one_arrives() {
        let store = leveler_storage::MemoryEventStore::default();
        let log = EventLog::new(&store, SessionId::generate());
        let summarizer = FakeSummarizer(Briefing::Produced("read src/lib.rs"));
        let (result, _) = assemble(&log, Some(&summarizer), 5, Some(20)).await;
        let context = result.expect("assembly");
        assert!(context.compacted);
        assert!(
            context
                .prior
                .iter()
                .any(|message| message.text_content().contains("read src/lib.rs"))
        );
    }

    /// At or below the quality boundary nothing is folded, even if a summary
    /// was somehow offered.
    #[tokio::test]
    async fn no_pressure_never_folds() {
        let store = leveler_storage::MemoryEventStore::default();
        let log = EventLog::new(&store, SessionId::generate());
        let summarizer = FakeSummarizer(Briefing::Produced("read src/lib.rs"));
        let (result, original) = assemble(&log, Some(&summarizer), 1_000, Some(5_000)).await;
        let context = result.expect("assembly");
        assert!(!context.compacted);
        assert_eq!(context.prior, original);
    }

    /// What the summarizer was asked for: the fold's own retention facts and
    /// the exact messages they select.
    struct RecordingSummarizer {
        seen: std::sync::Mutex<Option<(usize, u64, Vec<String>)>>,
    }

    #[async_trait::async_trait]
    impl ContextSummarizer for RecordingSummarizer {
        async fn summarize(
            &self,
            messages: &[Message],
            keep_recent: usize,
            keep_recent_tokens: u64,
        ) -> Result<Option<String>, EngineError> {
            *self.seen.lock().unwrap() = Some((
                keep_recent,
                keep_recent_tokens,
                messages.iter().map(|m| m.text_content()).collect(),
            ));
            Ok(Some("briefing".to_string()))
        }
    }

    /// The briefing is asked for with the FOLD's retention facts, and therefore
    /// covers every round the fold removes.
    ///
    /// The tail is count-bounded at four rounds, so a token budget narrower
    /// than it must drop the oldest of them; those dropped rounds are exactly
    /// what the briefing has to replace. A summarizer deriving its own retention
    /// (e.g. half the quality threshold) would stop earlier and leave the band
    /// between the two cuts in neither the request nor the briefing.
    #[tokio::test]
    async fn the_briefing_is_asked_for_the_rounds_the_fold_elides() {
        let store = leveler_storage::MemoryEventStore::default();
        let log = EventLog::new(&store, SessionId::generate());
        // head (User) + 19 light rounds + 4 heavy rounds (~4000 tokens each).
        let mut messages = vec![Message::text(Role::User, "the objective")];
        for index in 0..19 {
            messages.push(Message::text(Role::Assistant, format!("light {index}")));
        }
        for index in 0..4 {
            messages.push(Message::text(
                Role::Assistant,
                format!("heavy {index} {}", "h".repeat(16_000)),
            ));
        }
        let original = messages.clone();
        let raw = RawTranscript {
            messages,
            offset: 0,
        };
        let summarizer = RecordingSummarizer {
            seen: std::sync::Mutex::new(None),
        };
        let context = raw
            .assemble_measured(
                &log,
                Some(&summarizer),
                None,
                20, // the message count is the pressure figure in this fixture
                None,
                4,
                8_000,
                &count,
            )
            .await
            .expect("assembly");
        assert!(context.compacted, "the fixture must fold");

        let (keep_recent, keep_recent_tokens, seen) = summarizer
            .seen
            .lock()
            .unwrap()
            .take()
            .expect("a briefing was requested");
        assert_eq!(
            (keep_recent, keep_recent_tokens),
            (4, 8_000),
            "the briefing is asked with the fold's own retention facts"
        );
        assert_eq!(
            seen.len(),
            original.len(),
            "the summarizer owns the slice; the caller hands over the facts"
        );
        // The fold's actual cut, read from the folded surface: the trailing run
        // of `prior` that is still the original transcript IS the kept tail.
        let tail_len = context
            .prior
            .iter()
            .rev()
            .take_while(|message| original.contains(message))
            .count();
        let actual_tail_start = original.len() - tail_len;
        let (_, briefed_end) =
            leveler_context::compaction_span(&original, keep_recent, keep_recent_tokens)
                .expect("the fixture has a foldable middle");
        assert_eq!(
            actual_tail_start, briefed_end,
            "the fold cuts where the one span owner says"
        );
        assert!(actual_tail_start > 1, "the fixture really folds a middle");
        let elided = &original[1..actual_tail_start];
        let briefed = &seen[..briefed_end];
        assert!(
            elided
                .iter()
                .any(|message| message.text_content().starts_with("heavy 0 ")),
            "a heavy round the fold elides is in the fixture"
        );
        for message in elided {
            assert!(
                briefed.contains(&message.text_content()),
                "an elided round never reached the briefing: {:.40}",
                message.text_content()
            );
        }
    }
}
