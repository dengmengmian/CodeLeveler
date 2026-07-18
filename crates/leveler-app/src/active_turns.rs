//! Per-session ownership of active interactive turns.

use std::collections::HashMap;
use std::sync::Mutex;

use leveler_core::SessionId;
use tokio_util::sync::CancellationToken;

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub(crate) enum TurnAdmissionError {
    #[error("session {0} already has an active turn")]
    Busy(SessionId),
    #[error("interactive runtime is at its {0}-turn capacity")]
    Capacity(usize),
}

pub(crate) struct ActiveTurns {
    active: Mutex<HashMap<SessionId, CancellationToken>>,
    capacity: usize,
}

impl Default for ActiveTurns {
    fn default() -> Self {
        Self {
            active: Mutex::new(HashMap::new()),
            capacity: 4,
        }
    }
}

impl ActiveTurns {
    pub(crate) fn admit(
        &self,
        session_id: &SessionId,
    ) -> Result<CancellationToken, TurnAdmissionError> {
        let mut active = self.active.lock().unwrap();
        if active.contains_key(session_id) {
            return Err(TurnAdmissionError::Busy(session_id.clone()));
        }
        if active.len() >= self.capacity {
            return Err(TurnAdmissionError::Capacity(self.capacity));
        }
        let token = CancellationToken::new();
        active.insert(session_id.clone(), token.clone());
        Ok(token)
    }

    pub(crate) fn cancel(&self, session_id: &SessionId) -> bool {
        if let Some(token) = self.active.lock().unwrap().get(session_id) {
            token.cancel();
            true
        } else {
            false
        }
    }

    pub(crate) fn finish(&self, session_id: &SessionId) {
        self.active.lock().unwrap().remove(session_id);
    }

    pub(crate) fn cancel_all(&self) {
        for (_, token) in self.active.lock().unwrap().drain() {
            token.cancel();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn same_session_has_exactly_one_active_turn() {
        let turns = ActiveTurns::default();
        let session = SessionId::new("a");
        let first = turns.admit(&session).unwrap();
        assert!(matches!(
            turns.admit(&session),
            Err(TurnAdmissionError::Busy(id)) if id == session
        ));
        assert!(
            !first.is_cancelled(),
            "rejected admission must not replace it"
        );
    }

    #[test]
    fn cancel_is_scoped_to_the_target_session() {
        let turns = ActiveTurns::default();
        let a = SessionId::new("a");
        let b = SessionId::new("b");
        let token_a = turns.admit(&a).unwrap();
        let token_b = turns.admit(&b).unwrap();

        assert!(turns.cancel(&a));
        assert!(token_a.is_cancelled());
        assert!(!token_b.is_cancelled());
        assert!(!turns.cancel(&SessionId::new("missing")));
    }

    #[test]
    fn capacity_is_explicit_and_finishing_releases_it() {
        let turns = ActiveTurns {
            active: Mutex::new(HashMap::new()),
            capacity: 2,
        };
        let a = SessionId::new("a");
        let b = SessionId::new("b");
        turns.admit(&a).unwrap();
        turns.admit(&b).unwrap();
        assert!(matches!(
            turns.admit(&SessionId::new("c")),
            Err(TurnAdmissionError::Capacity(2))
        ));
        turns.finish(&a);
        assert!(turns.admit(&SessionId::new("c")).is_ok());
    }
}
