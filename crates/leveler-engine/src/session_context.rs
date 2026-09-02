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

use leveler_core::SessionId;
use leveler_storage::MessageStore;

use crate::engine::{PriorMerge, fold_prior_messages, merge_prior_messages};
use crate::log::EventLog;
use crate::{EngineError, EngineEvent};

/// Produces the model handoff briefing for messages about to be folded away.
/// [`RawTranscript::assemble`] asks for one only after merging the latest
/// snapshot with its tail still leaves the context over threshold — so a
/// turn that fits from the snapshot never pays for a summary it would drop.
#[async_trait::async_trait]
pub trait ContextSummarizer: Send + Sync {
    async fn summarize(&self, messages: &[leveler_model::Message]) -> Option<String>;
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
    pub async fn assemble(
        self,
        log: &EventLog<'_>,
        summarizer: Option<&dyn ContextSummarizer>,
        active_objective: Option<&str>,
        threshold: u64,
    ) -> Result<SessionContext, EngineError> {
        let through_ordinal = self.messages.len() as u64;
        let snapshot = log.latest_context_snapshot(None).await?;
        let (prior, compacted) = match merge_prior_messages(self.messages, snapshot, threshold) {
            (base, PriorMerge::Fits { merged }) => (base, merged),
            (base, PriorMerge::Over { base_tokens }) => {
                let summary = match summarizer {
                    Some(summarizer) => summarizer.summarize(&base).await,
                    None => None,
                };
                fold_prior_messages(
                    base,
                    base_tokens,
                    summary.as_deref(),
                    active_objective,
                    threshold,
                )
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
