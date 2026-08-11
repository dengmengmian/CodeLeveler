//! Conversation checkpoints: the one owner of the checkpoint list AND its
//! workspace snapshots.
//!
//! The two maps carry a joint invariant — a listed checkpoint's workspace
//! snapshot (when one was captured) must be resolvable by its id, and a
//! dropped checkpoint must not strand its snapshot entry. Holding both maps
//! in one type makes that invariant local instead of a convention spread
//! across the runtime client and the detached compact worker.

use std::collections::HashMap;
use std::sync::Mutex;

use leveler_client_protocol::UiCheckpoint;
use leveler_core::CheckpointId;
use leveler_core::SessionId;

/// Per-session conversation checkpoints + their captured workspace snapshots.
/// Shared via `Arc` between the runtime client and detached context workers.
#[derive(Default)]
pub(crate) struct CheckpointStore {
    checkpoints: Mutex<HashMap<SessionId, Vec<UiCheckpoint>>>,
    snapshots: Mutex<HashMap<CheckpointId, leveler_execution::SnapshotId>>,
}

impl CheckpointStore {
    /// Record a checkpoint, registering its workspace snapshot (if captured)
    /// BEFORE listing the checkpoint, so a listed id is always resolvable.
    pub fn record(
        &self,
        session_id: &SessionId,
        checkpoint: UiCheckpoint,
        snapshot: Option<leveler_execution::SnapshotId>,
    ) {
        if let Some(snapshot) = snapshot {
            self.snapshots
                .lock()
                .unwrap()
                .insert(checkpoint.id.clone(), snapshot);
        }
        self.checkpoints
            .lock()
            .unwrap()
            .entry(session_id.clone())
            .or_default()
            .push(checkpoint);
    }

    /// The session's checkpoints, oldest first (for the reconnect snapshot).
    pub fn list(&self, session_id: &SessionId) -> Vec<UiCheckpoint> {
        self.checkpoints
            .lock()
            .unwrap()
            .get(session_id)
            .cloned()
            .unwrap_or_default()
    }

    /// The transcript ordinal a checkpoint restores to, if it still exists.
    pub fn ordinal_of(&self, session_id: &SessionId, id: &CheckpointId) -> Option<u32> {
        self.checkpoints
            .lock()
            .unwrap()
            .get(session_id)
            .into_iter()
            .flatten()
            .find(|c| &c.id == id)
            .map(|c| c.ordinal)
    }

    /// The workspace snapshot captured for a checkpoint, if any (non-git
    /// workspaces have transcript-only checkpoints).
    pub fn snapshot_of(&self, id: &CheckpointId) -> Option<leveler_execution::SnapshotId> {
        self.snapshots.lock().unwrap().get(id).cloned()
    }

    /// After a restore to `restored_ordinal`, drop later checkpoints (and
    /// their snapshots) so the UI cannot re-restore a point that no longer
    /// exists in the transcript.
    pub fn prune_after_restore(&self, session_id: &SessionId, restored_ordinal: u32) {
        let discarded = {
            let mut map = self.checkpoints.lock().unwrap();
            let Some(list) = map.get_mut(session_id) else {
                return;
            };
            let mut keep = Vec::new();
            let mut drop = Vec::new();
            for checkpoint in list.drain(..) {
                if checkpoint.ordinal <= restored_ordinal {
                    keep.push(checkpoint);
                } else {
                    drop.push(checkpoint);
                }
            }
            *list = keep;
            drop
        };
        if discarded.is_empty() {
            return;
        }
        let mut snaps = self.snapshots.lock().unwrap();
        for checkpoint in discarded {
            snaps.remove(&checkpoint.id);
        }
    }

    /// Drop every checkpoint (and snapshot) for a session. Used after /clear
    /// and /compact, when prior ordinals no longer describe the transcript.
    pub fn drop_session(&self, session_id: &SessionId) {
        let removed = self
            .checkpoints
            .lock()
            .unwrap()
            .remove(session_id)
            .unwrap_or_default();
        if removed.is_empty() {
            return;
        }
        let mut snaps = self.snapshots.lock().unwrap();
        for checkpoint in removed {
            snaps.remove(&checkpoint.id);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cp(id: &str, ordinal: u32) -> UiCheckpoint {
        UiCheckpoint {
            id: CheckpointId::new(id),
            label: format!("cp-{id}"),
            ordinal,
        }
    }

    #[test]
    fn prune_drops_later_checkpoints_and_their_snapshots() {
        let store = CheckpointStore::default();
        let session = SessionId::new("s1");
        store.record(
            &session,
            cp("a", 2),
            Some(leveler_execution::SnapshotId("snap-a".into())),
        );
        store.record(
            &session,
            cp("b", 5),
            Some(leveler_execution::SnapshotId("snap-b".into())),
        );
        store.prune_after_restore(&session, 2);
        assert_eq!(store.list(&session).len(), 1);
        assert!(store.snapshot_of(&CheckpointId::new("a")).is_some());
        assert!(
            store.snapshot_of(&CheckpointId::new("b")).is_none(),
            "a pruned checkpoint must not strand its snapshot"
        );
    }

    #[test]
    fn drop_session_clears_both_maps_and_spares_other_sessions() {
        let store = CheckpointStore::default();
        let session = SessionId::new("s1");
        let other = SessionId::new("other");
        store.record(
            &session,
            cp("a", 1),
            Some(leveler_execution::SnapshotId("snap-a".into())),
        );
        store.record(
            &other,
            cp("o", 1),
            Some(leveler_execution::SnapshotId("snap-o".into())),
        );
        store.drop_session(&session);
        assert!(store.list(&session).is_empty());
        assert!(store.snapshot_of(&CheckpointId::new("a")).is_none());
        assert_eq!(store.list(&other).len(), 1, "other sessions keep theirs");
        assert!(store.snapshot_of(&CheckpointId::new("o")).is_some());
    }
}
