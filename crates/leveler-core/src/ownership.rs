//! Task ownership vocabulary: the fencing epoch and the token a runtime
//! holds while executing a task.
//!
//! `RuntimeId` is the durable host identity; [`OwnerEpoch`] is the ownership
//! *incarnation*. The same runtime re-acquiring a task after a restart gets a
//! higher epoch, so tokens minted by the previous process become powerless
//! even though the runtime identity is unchanged. Ownership is proven by a
//! current token, never by process existence.

use serde::{Deserialize, Serialize};

use crate::ids::{RuntimeId, TaskId};

/// A monotonic fencing token generation. Epoch 0 means "never owned"; the
/// first acquisition yields epoch 1. Epochs only move forward — there is no
/// decrement and no wrap-around: exhausting the space is a hard error, never
/// a rollback to an old epoch a stale runtime might still hold.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, Default,
)]
pub struct OwnerEpoch(u64);

/// Advancing past this fails loudly: epochs are persisted in an SQLite
/// `INTEGER` (i64), and a fencing token must never be truncated or wrapped
/// into a value an old owner could still hold.
const MAX_PERSISTABLE_EPOCH: u64 = i64::MAX as u64;

impl OwnerEpoch {
    /// The "never owned" epoch.
    pub const UNOWNED: OwnerEpoch = OwnerEpoch(0);

    /// Wrap a persisted epoch value.
    pub fn new(value: u64) -> Self {
        Self(value)
    }

    /// The raw value (persistence).
    pub fn get(self) -> u64 {
        self.0
    }

    /// The next epoch. `None` when the persistable space is exhausted —
    /// callers must fail loudly; a fencing token never wraps.
    #[must_use]
    pub fn next(self) -> Option<OwnerEpoch> {
        if self.0 >= MAX_PERSISTABLE_EPOCH {
            return None;
        }
        Some(OwnerEpoch(self.0 + 1))
    }
}

impl std::fmt::Display for OwnerEpoch {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// Proof of ownership a runtime carries while executing a task: who I am,
/// which task I own, and which ownership generation I hold. Every fenced
/// authoritative write presents this token; a token whose epoch is no longer
/// the task's current epoch is stale and must be rejected.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OwnershipToken {
    pub task_id: TaskId,
    pub runtime_id: RuntimeId,
    pub owner_epoch: OwnerEpoch,
}

impl std::fmt::Display for OwnershipToken {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{}@{}#{}",
            self.task_id, self.runtime_id, self.owner_epoch
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn epochs_are_ordered_and_monotonic() {
        let e1 = OwnerEpoch::UNOWNED.next().unwrap();
        let e2 = e1.next().unwrap();
        assert!(OwnerEpoch::UNOWNED < e1);
        assert!(e1 < e2);
        assert_eq!(e1.get(), 1);
        assert_eq!(e2.get(), 2);
    }

    #[test]
    fn epoch_exhaustion_fails_instead_of_wrapping() {
        let max = OwnerEpoch::new(i64::MAX as u64);
        assert_eq!(max.next(), None, "a fencing token must never wrap");
        assert_eq!(OwnerEpoch::new(u64::MAX).next(), None);
    }

    #[test]
    fn token_serde_round_trips() {
        let token = OwnershipToken {
            task_id: TaskId::new("t1"),
            runtime_id: RuntimeId::new("rt-a"),
            owner_epoch: OwnerEpoch::new(7),
        };
        let json = serde_json::to_string(&token).unwrap();
        let back: OwnershipToken = serde_json::from_str(&json).unwrap();
        assert_eq!(back, token);
        assert_eq!(token.to_string(), "t1@rt-a#7");
    }
}
