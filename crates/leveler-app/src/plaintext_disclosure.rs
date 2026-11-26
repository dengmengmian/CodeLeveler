//! Per-input plaintext disclosure permits.
//!
//! The product lets a person explicitly choose "send as plain text" for one
//! input. That decision is a real disclosure to the model and the provider, so
//! it cannot be re-derived from the text (a registered value is normally
//! scrubbed) and it must never become a session-wide switch. This store keeps
//! the decision next to the input it belongs to:
//!
//! - the permit is memory-only; the session journal, transcript, events and
//!   every other durable channel keep the redacted projection;
//! - an input permit is bound to one journal row and its revision, so an edit,
//!   withdraw, or a later unrelated turn cannot reuse it;
//! - the opening-message permit is consumed by the turn that takes it, so a
//!   failed or abandoned turn cannot hand it to the next one;
//! - the store is bounded per session. A permit that does not fit is simply not
//!   granted, which falls back to the safe redacted projection.
//!
//! The value is still registered in the session secret registry by the caller,
//! so a paraphrase the model writes later is scrubbed from durable text like
//! any other registered value. This store only widens what the *authorized
//! model request for that one input* may see.

use std::collections::{HashMap, VecDeque};
use std::sync::Mutex;

/// How many pending input permits one session may hold. A session cannot have
/// more than a few pending steers in practice; the bound exists so a bug or a
/// hostile client cannot turn the store into an unbounded plaintext buffer.
const MAX_INPUTS_PER_SESSION: usize = 16;

/// A single input text is already bounded by the admission path (256 KiB for a
/// steer). The seed may be a long document; keep a hard ceiling anyway.
const MAX_TEXT_BYTES: usize = 1024 * 1024;

#[derive(Default)]
struct SessionDisclosures {
    /// The opening message of the session's next turn, when it is not an
    /// accepted steer promoted from the journal.
    seed: Option<String>,
    /// A `/goal <task>` declaration injected into a running turn.
    goal: Option<String>,
    inputs: HashMap<String, (i64, String)>,
    order: VecDeque<String>,
}

/// Process-local, per-session plaintext permits. Never persisted.
#[derive(Default)]
pub(crate) struct PlaintextDisclosures {
    sessions: Mutex<HashMap<String, SessionDisclosures>>,
}

impl PlaintextDisclosures {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// The opening message for the next turn. Replaces any previous seed: one
    /// session has one live turn, and a stale seed must not be inherited.
    pub(crate) fn grant_seed(&self, session: &str, text: &str) {
        if session.is_empty() || !fits(text) {
            return;
        }
        let mut sessions = self.sessions.lock().unwrap_or_else(|e| e.into_inner());
        sessions.entry(session.to_string()).or_default().seed = Some(text.to_string());
    }

    /// A `/goal <task>` declaration for the running turn.
    pub(crate) fn grant_goal(&self, session: &str, text: &str) {
        if session.is_empty() || !fits(text) {
            return;
        }
        let mut sessions = self.sessions.lock().unwrap_or_else(|e| e.into_inner());
        sessions.entry(session.to_string()).or_default().goal = Some(text.to_string());
    }

    /// A steer journal row's exact admitted text. Bound to the revision that
    /// was current when the person made the decision.
    pub(crate) fn grant_input(&self, session: &str, input_id: &str, revision: i64, text: &str) {
        if session.is_empty() || input_id.is_empty() || !fits(text) {
            return;
        }
        let mut sessions = self.sessions.lock().unwrap_or_else(|e| e.into_inner());
        let entry = sessions.entry(session.to_string()).or_default();
        if !entry.inputs.contains_key(input_id) {
            while entry.order.len() >= MAX_INPUTS_PER_SESSION {
                let Some(oldest) = entry.order.pop_front() else {
                    break;
                };
                entry.inputs.remove(&oldest);
            }
            entry.order.push_back(input_id.to_string());
        }
        entry
            .inputs
            .insert(input_id.to_string(), (revision, text.to_string()));
    }

    /// Take the opening-message permit for this seed. `input_id` is the
    /// journal identity for a promoted steer; `None` means the turn's own
    /// opening message. One-shot: the turn that takes it is the only consumer.
    pub(crate) fn take_seed(&self, session: &str, input_id: Option<&str>) -> Option<String> {
        let mut sessions = self.sessions.lock().unwrap_or_else(|e| e.into_inner());
        let entry = sessions.get_mut(session)?;
        match input_id {
            Some(id) => {
                let (_, text) = entry.inputs.remove(id)?;
                entry.order.retain(|known| known != id);
                Some(text)
            }
            None => entry.seed.take(),
        }
    }

    /// The permit for a pending steer row, only while it is still the exact
    /// revision the person authorized. A read, not a take: the row may be
    /// delivered once, and [`Self::remove_input`] ends the permit then.
    pub(crate) fn input_text(
        &self,
        session: &str,
        input_id: &str,
        revision: i64,
    ) -> Option<String> {
        let sessions = self.sessions.lock().unwrap_or_else(|e| e.into_inner());
        let (authorized_revision, text) = sessions.get(session)?.inputs.get(input_id)?;
        (*authorized_revision == revision).then(|| text.clone())
    }

    /// Take the goal-declaration permit only when it is the text being
    /// injected. A different declaration is never handed another's decision.
    pub(crate) fn take_goal_if_matches(&self, session: &str, text: &str) -> Option<String> {
        let mut sessions = self.sessions.lock().unwrap_or_else(|e| e.into_inner());
        let entry = sessions.get_mut(session)?;
        if entry.goal.as_deref() == Some(text) {
            entry.goal.take()
        } else {
            None
        }
    }

    /// The input settled (delivered, edited, withdrawn or cancelled): its
    /// decision is spent.
    pub(crate) fn remove_input(&self, session: &str, input_id: &str) {
        let mut sessions = self.sessions.lock().unwrap_or_else(|e| e.into_inner());
        let Some(entry) = sessions.get_mut(session) else {
            return;
        };
        entry.inputs.remove(input_id);
        entry.order.retain(|known| known != input_id);
    }

    /// A new turn is being admitted: clear the opening-message and goal slots
    /// so a permit granted for a turn that never consumed it cannot be
    /// inherited by this one. Input permits stay: they belong to journal rows
    /// that are still pending and may be delivered by this turn.
    pub(crate) fn clear_turn_slots(&self, session: &str) {
        let mut sessions = self.sessions.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(entry) = sessions.get_mut(session) {
            entry.seed = None;
            entry.goal = None;
        }
    }
}

fn fits(text: &str) -> bool {
    !text.is_empty() && text.len() <= MAX_TEXT_BYTES
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_input_permit_is_bound_to_the_revision_it_was_granted_for() {
        let store = PlaintextDisclosures::new();
        store.grant_input("s", "i1", 3, "key: canary-value");
        assert_eq!(
            store.input_text("s", "i1", 3).as_deref(),
            Some("key: canary-value")
        );
        assert_eq!(
            store.input_text("s", "i1", 4),
            None,
            "an edited input is a different text the person never authorized"
        );
    }

    #[test]
    fn a_seed_permit_is_one_shot_and_never_crosses_sessions() {
        let store = PlaintextDisclosures::new();
        store.grant_seed("s", "one");
        assert_eq!(store.take_seed("s", None).as_deref(), Some("one"));
        assert_eq!(store.take_seed("s", None), None);
        store.grant_seed("s", "one");
        assert_eq!(store.take_seed("other", None), None);
        assert_eq!(store.take_seed("s", None).as_deref(), Some("one"));
    }

    #[test]
    fn a_promoted_seed_uses_its_input_permit_once() {
        let store = PlaintextDisclosures::new();
        store.grant_input("s", "i1", 1, "secret text");
        assert_eq!(
            store.take_seed("s", Some("i1")).as_deref(),
            Some("secret text")
        );
        assert_eq!(store.take_seed("s", Some("i1")), None);
    }

    #[test]
    fn a_goal_permit_only_matches_the_declaration_it_was_granted_for() {
        let store = PlaintextDisclosures::new();
        store.grant_goal("s", "goal one");
        assert_eq!(store.take_goal_if_matches("s", "goal two"), None);
        assert_eq!(
            store.take_goal_if_matches("s", "goal one").as_deref(),
            Some("goal one")
        );
        assert_eq!(store.take_goal_if_matches("s", "goal one"), None);
    }

    #[test]
    fn removing_an_input_ends_its_permit() {
        let store = PlaintextDisclosures::new();
        store.grant_input("s", "i1", 1, "secret");
        store.remove_input("s", "i1");
        assert_eq!(store.input_text("s", "i1", 1), None);
    }

    #[test]
    fn the_store_is_bounded_and_evicts_the_oldest_input() {
        let store = PlaintextDisclosures::new();
        for index in 0..=MAX_INPUTS_PER_SESSION {
            store.grant_input("s", &format!("i{index}"), 0, "secret");
        }
        assert_eq!(store.input_text("s", "i0", 0), None);
        assert_eq!(
            store
                .input_text("s", &format!("i{MAX_INPUTS_PER_SESSION}"), 0)
                .as_deref(),
            Some("secret")
        );
    }

    #[test]
    fn a_new_turn_cannot_inherit_an_unconsumed_seed_or_goal_slot() {
        let store = PlaintextDisclosures::new();
        store.grant_seed("s", "seed");
        store.grant_goal("s", "goal");
        store.clear_turn_slots("s");
        assert_eq!(store.take_seed("s", None), None);
        assert_eq!(store.take_goal_if_matches("s", "goal"), None);
    }

    #[test]
    fn oversized_text_is_never_kept() {
        let store = PlaintextDisclosures::new();
        let huge = "x".repeat(MAX_TEXT_BYTES + 1);
        store.grant_seed("s", &huge);
        assert_eq!(store.take_seed("s", None), None);
    }
}
