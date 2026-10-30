//! Correlation and refresh discipline for the read-only pages.
//!
//! A live diagnostic page (`/trace`, `/context`, `/usage`) asks the runtime for
//! a projection and re-asks when the runtime reports a fact that changes it.
//! Two failures are easy to build by accident and both look like "the page does
//! not work":
//!
//! - **Dropping answers.** A page that stores a single `pending_id` and
//!   overwrites it on the next event silently discards the answer to a query it
//!   really sent. Under a busy turn (tool start/finish, per-round usage) this
//!   can drop every answer in a burst, leaving the page on "loading" while the
//!   runtime answered every time.
//! - **Flooding.** One query per event turns a chatty turn into a stream of
//!   database reads whose answers race each other, and it makes the delivery
//!   worker the bottleneck for every other command the user types.
//!
//! [`QueryTrack`] makes both impossible: at most one query is in flight, later
//! events only set a dirty flag, and the follow-up query is issued when the
//! answer lands. A user-initiated query (opening the page, `r`, a centered
//! window) replaces the in-flight one, and only the answer to the newest query
//! is applied.

use leveler_client_protocol::CommandId;

#[derive(Debug, Clone, Default)]
pub struct QueryTrack {
    /// The one query whose answer this page will accept.
    in_flight: Option<CommandId>,
    /// A fact changed while a query was in flight; ask again when it lands.
    dirty: bool,
}

impl QueryTrack {
    /// Start a query, returning its correlation id. Any earlier in-flight query
    /// is superseded: its answer will be ignored as stale.
    pub fn issue(&mut self) -> CommandId {
        let id = CommandId::generate();
        self.in_flight = Some(id.clone());
        self.dirty = false;
        id
    }

    /// Note that the page's data is out of date without issuing anything yet.
    pub fn mark_dirty(&mut self) {
        self.dirty = true;
    }

    /// Whether a follow-up query is owed right now: the page is dirty and
    /// nothing is outstanding. The caller mints the query itself, so the
    /// request keeps its one constructor.
    pub fn wants_refresh(&self) -> bool {
        self.dirty && self.in_flight.is_none()
    }

    /// The follow-up query for a dirty page, unless one is already in flight.
    ///
    /// This is the whole coalescing rule: a refresh either starts now because
    /// nothing is pending, or waits for the outstanding answer, which will
    /// leave the flag set for the next pass.
    pub fn refresh_if_idle(&mut self) -> Option<CommandId> {
        self.wants_refresh().then(|| self.issue())
    }

    /// Accept an answer only when it belongs to the query this page is waiting
    /// for. Returns whether the payload should be applied.
    pub fn accept(&mut self, query_id: Option<&CommandId>) -> bool {
        let Some(incoming) = query_id else {
            return false;
        };
        if self.in_flight.as_ref() != Some(incoming) {
            return false;
        }
        self.in_flight = None;
        true
    }

    /// The correlation id this page is waiting for, if any.
    pub fn in_flight_id(&self) -> Option<CommandId> {
        self.in_flight.clone()
    }

    /// Whether an answer is still outstanding (the page is loading).
    pub fn in_flight(&self) -> bool {
        self.in_flight.is_some()
    }

    /// Whether a refresh is owed once the outstanding answer lands.
    pub fn is_dirty(&self) -> bool {
        self.dirty
    }

    /// Forget both the outstanding query and any pending refresh.
    ///
    /// Used when the page closes: a page that is not on screen must not keep a
    /// query id alive, or a response arriving later would be applied to a view
    /// nobody is looking at and would hold the next open in "loading".
    pub fn reset(&mut self) {
        self.in_flight = None;
        self.dirty = false;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_answer_is_applied_only_for_the_query_in_flight() {
        let mut track = QueryTrack::default();
        let id = track.issue();
        assert!(track.in_flight());
        // A foreign id (another client's answer) is ignored.
        assert!(!track.accept(Some(&CommandId::generate())));
        assert!(track.in_flight(), "the real answer is still owed");
        assert!(track.accept(Some(&id)));
        assert!(!track.in_flight());
        // A replay of the same answer is not applied twice.
        assert!(!track.accept(Some(&id)));
    }

    #[test]
    fn events_during_a_flight_coalesce_into_one_follow_up() {
        let mut track = QueryTrack::default();
        let first = track.issue();
        track.mark_dirty();
        track.mark_dirty();
        assert!(
            track.refresh_if_idle().is_none(),
            "no second query while one is in flight"
        );
        assert!(track.accept(Some(&first)));
        // Exactly one follow-up covers both events.
        assert!(track.refresh_if_idle().is_some());
        assert!(track.refresh_if_idle().is_none());
    }

    #[test]
    fn the_follow_up_query_actually_runs_when_the_answer_lands() {
        let mut track = QueryTrack::default();
        let first = track.issue();
        track.mark_dirty();
        assert!(track.refresh_if_idle().is_none());
        assert!(track.accept(Some(&first)));
        let second = track.refresh_if_idle().expect("the owed refresh runs");
        assert_ne!(first, second);
        assert!(!track.is_dirty(), "issuing spent the flag");
    }

    #[test]
    fn idle_page_does_not_query_without_news() {
        let mut track = QueryTrack::default();
        assert!(track.refresh_if_idle().is_none());
    }

    #[test]
    fn closing_resets_both_the_flight_and_the_owe() {
        let mut track = QueryTrack::default();
        let id = track.issue();
        track.mark_dirty();
        track.reset();
        assert!(!track.in_flight());
        assert!(!track.is_dirty());
        // A late answer to the closed page's query is not applied.
        assert!(!track.accept(Some(&id)));
    }

    #[test]
    fn a_replaced_query_makes_the_old_answer_stale() {
        let mut track = QueryTrack::default();
        let old = track.issue();
        let new = track.issue();
        assert!(
            !track.accept(Some(&old)),
            "the superseded answer must not overwrite the newer request"
        );
        assert!(track.accept(Some(&new)));
    }

    #[test]
    fn a_peer_that_does_not_echo_ids_is_never_mistaken_for_ownership() {
        let mut track = QueryTrack::default();
        let _ = track.issue();
        assert!(!track.accept(None));
        assert!(track.in_flight());
        let _ = track.issue();
    }
}
