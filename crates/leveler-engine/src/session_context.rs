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
use leveler_storage::{Database, MessageRepository};

use crate::log::EventLog;
use crate::{EngineError, EngineEvent, budget_prior_messages};

/// The parsed transcript of a session, in ordinal order.
pub struct RawTranscript {
    pub messages: Vec<leveler_model::Message>,
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
        db: &Database,
        session_id: &SessionId,
        what: &str,
    ) -> Result<Self, EngineError> {
        let payloads = MessageRepository::new(db).load(session_id).await?;
        let messages = payloads
            .iter()
            .map(|p| serde_json::from_str(p))
            .collect::<Result<Vec<leveler_model::Message>, _>>()
            .map_err(|error| EngineError::Corrupt(format!("unreplayable {what}: {error}")))?;
        Ok(Self { messages })
    }

    /// Lossy load: an unreadable legacy row only loses context (interactive
    /// chat and side questions tolerate that; resume must not).
    pub async fn load_lossy(db: &Database, session_id: &SessionId) -> Result<Self, EngineError> {
        let payloads = MessageRepository::new(db).load(session_id).await?;
        let messages = payloads
            .iter()
            .filter_map(|p| serde_json::from_str(p).ok())
            .collect();
        Ok(Self { messages })
    }

    pub fn is_empty(&self) -> bool {
        self.messages.is_empty()
    }

    /// Assemble the model-visible context: latest snapshot (with watermark)
    /// plus the post-snapshot tail, folded under `threshold` if needed.
    /// `summary` is an optional model handoff briefing the caller produced
    /// for oversized histories.
    pub async fn assemble(
        self,
        log: &EventLog<'_>,
        summary: Option<&str>,
        active_objective: Option<&str>,
        threshold: u64,
    ) -> Result<SessionContext, EngineError> {
        let through_ordinal = self.messages.len() as u64;
        let snapshot = log.latest_context_snapshot(None).await?;
        let (prior, compacted) = budget_prior_messages(
            self.messages,
            snapshot,
            summary,
            active_objective,
            threshold,
        );
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
