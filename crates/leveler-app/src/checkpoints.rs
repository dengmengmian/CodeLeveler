//! Conversation checkpoints and their workspace snapshots share one lifetime.

use std::collections::HashMap;
use std::sync::Mutex;

use leveler_client_protocol::UiCheckpoint;
use leveler_core::{CheckpointId, SessionId};
use leveler_execution::SnapshotId;

#[derive(Clone)]
pub(crate) struct Checkpoint {
    pub view: UiCheckpoint,
    pub event_cursor: i64,
    pub workspace: Option<SnapshotId>,
}

/// Per-session conversation checkpoints, oldest first. The transcript position
/// and durable event cursor describe the same pre-turn boundary.
#[derive(Default)]
pub(crate) struct CheckpointStore(Mutex<HashMap<SessionId, Vec<Checkpoint>>>);

impl CheckpointStore {
    pub fn record(&self, session_id: &SessionId, checkpoint: Checkpoint) {
        self.0
            .lock()
            .unwrap()
            .entry(session_id.clone())
            .or_default()
            .push(checkpoint);
    }

    pub fn list(&self, session_id: &SessionId) -> Vec<UiCheckpoint> {
        self.0
            .lock()
            .unwrap()
            .get(session_id)
            .into_iter()
            .flatten()
            .map(|checkpoint| checkpoint.view.clone())
            .collect()
    }

    pub fn get(&self, session_id: &SessionId, id: &CheckpointId) -> Option<Checkpoint> {
        self.0
            .lock()
            .unwrap()
            .get(session_id)?
            .iter()
            .find(|checkpoint| &checkpoint.view.id == id)
            .cloned()
    }

    /// Checkpoints created after this identity belong to the abandoned suffix,
    /// even if they happened to share a message ordinal or event cursor.
    pub fn prune_after_restore(&self, session_id: &SessionId, id: &CheckpointId) {
        let mut map = self.0.lock().unwrap();
        if let Some(list) = map.get_mut(session_id)
            && let Some(index) = list.iter().position(|checkpoint| &checkpoint.view.id == id)
        {
            list.truncate(index + 1);
        }
    }

    /// Drop all checkpoints after /clear or /compact replaces their transcript.
    pub fn drop_session(&self, session_id: &SessionId) {
        self.0.lock().unwrap().remove(session_id);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cp(id: &str, ordinal: u32) -> Checkpoint {
        Checkpoint {
            view: UiCheckpoint {
                id: CheckpointId::new(id),
                label: format!("cp-{id}"),
                ordinal,
            },
            event_cursor: i64::from(ordinal),
            workspace: Some(SnapshotId(format!("snap-{id}"))),
        }
    }

    #[test]
    fn prune_drops_later_checkpoints_and_their_snapshots() {
        let store = CheckpointStore::default();
        let session = SessionId::new("s1");
        let retained = CheckpointId::new("a");
        store.record(&session, cp("a", 2));
        store.record(&session, cp("b", 2));
        store.record(&session, cp("c", 5));
        store.prune_after_restore(&session, &retained);
        assert_eq!(store.list(&session).len(), 1);
        assert_eq!(
            store.get(&session, &retained).unwrap().workspace.unwrap().0,
            "snap-a"
        );
        for removed in ["b", "c"] {
            assert!(store.get(&session, &CheckpointId::new(removed)).is_none());
        }
    }

    #[test]
    fn drop_session_clears_both_maps_and_spares_other_sessions() {
        let store = CheckpointStore::default();
        let session = SessionId::new("s1");
        let other = SessionId::new("other");
        store.record(&session, cp("a", 1));
        store.record(&other, cp("o", 1));
        store.drop_session(&session);
        assert!(store.list(&session).is_empty());
        assert!(store.get(&session, &CheckpointId::new("a")).is_none());
        assert_eq!(store.list(&other).len(), 1, "other sessions keep theirs");
        assert!(
            store
                .get(&other, &CheckpointId::new("o"))
                .unwrap()
                .workspace
                .is_some()
        );
    }
}
