//! The `OwnershipStore` port: durable task ownership with fencing epochs.
//!
//! Semantics, not CRUD: `acquire` is a compare-and-swap on the current epoch
//! — a single conditional UPDATE in SQLite, one winner under concurrency,
//! epoch strictly monotonic. There is no blind steal (the caller must name
//! the epoch it saw), no lease/TTL/heartbeat (later phases), and a stale
//! expectation is a typed [`OwnershipError::Stale`], never a generic
//! database error.

use std::collections::HashMap;
use std::sync::Mutex;

use async_trait::async_trait;

use leveler_core::{OwnerEpoch, OwnershipToken, RuntimeId, TaskId};

use crate::{Database, StorageError};

/// A task's current ownership state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TaskOwner {
    /// The owning runtime; `None` while unowned (epoch history retained).
    pub runtime: Option<RuntimeId>,
    /// Current fencing epoch (0 = never owned).
    pub epoch: OwnerEpoch,
}

/// Ownership operations that fail for ownership reasons carry them typed, so
/// callers can distinguish a fencing failure from a transient storage fault.
#[derive(Debug, thiserror::Error)]
pub enum OwnershipError {
    /// The caller's expectation no longer matches the task's current owner
    /// state — the caller is stale (or lost a race) and must not proceed.
    #[error(
        "stale ownership for task {task_id}: expected epoch {expected_epoch}, \
         current owner {actual_runtime:?} at epoch {actual_epoch}"
    )]
    Stale {
        /// The task whose ownership was contested.
        task_id: TaskId,
        /// The epoch the caller believed was current.
        expected_epoch: OwnerEpoch,
        /// The actual current owner runtime (None = unowned).
        actual_runtime: Option<RuntimeId>,
        /// The actual current epoch.
        actual_epoch: OwnerEpoch,
    },
    /// The epoch space is exhausted. Practically unreachable; failing loudly
    /// is the only acceptable behavior (a wrapped epoch would resurrect old
    /// tokens).
    #[error("owner epoch exhausted for task {task_id}")]
    EpochExhausted {
        /// The task whose epoch space ran out.
        task_id: TaskId,
    },
    /// The task does not exist.
    #[error("task {task_id} not found for ownership operation")]
    UnknownTask {
        /// The unknown task id.
        task_id: TaskId,
    },
    /// The underlying store failed.
    #[error(transparent)]
    Storage(#[from] StorageError),
}

/// Durable task-ownership access.
#[async_trait]
pub trait OwnershipStore: Send + Sync {
    /// The task's current owner state; `None` when the task does not exist.
    async fn current(&self, task_id: &TaskId) -> Result<Option<TaskOwner>, StorageError>;

    /// Compare-and-acquire: if the task's current epoch equals
    /// `expected_epoch`, atomically set `runtime` as owner at
    /// `expected_epoch + 1` and return the new token. Any mismatch is
    /// [`OwnershipError::Stale`] with the actual state. Exactly one of two
    /// concurrent callers with the same expectation wins.
    async fn acquire(
        &self,
        task_id: &TaskId,
        runtime: &RuntimeId,
        expected_epoch: OwnerEpoch,
    ) -> Result<OwnershipToken, OwnershipError>;
}

/// The production SQLite adapter over the `tasks` ownership columns
/// (migration 0017). The CAS is one conditional UPDATE — atomic by
/// construction, no check-then-write window.
#[async_trait]
impl OwnershipStore for Database {
    async fn current(&self, task_id: &TaskId) -> Result<Option<TaskOwner>, StorageError> {
        let row: Option<(Option<String>, i64)> =
            sqlx::query_as("SELECT owner_runtime_id, owner_epoch FROM tasks WHERE id = ?1")
                .bind(task_id.as_str())
                .fetch_optional(self.pool())
                .await?;
        Ok(row.map(|(runtime, epoch)| TaskOwner {
            runtime: runtime.map(RuntimeId::new),
            epoch: OwnerEpoch::new(epoch.max(0) as u64),
        }))
    }

    async fn acquire(
        &self,
        task_id: &TaskId,
        runtime: &RuntimeId,
        expected_epoch: OwnerEpoch,
    ) -> Result<OwnershipToken, OwnershipError> {
        let next = expected_epoch
            .next()
            .ok_or_else(|| OwnershipError::EpochExhausted {
                task_id: task_id.clone(),
            })?;
        let updated = sqlx::query(
            "UPDATE tasks SET owner_runtime_id = ?2, owner_epoch = ?3 \
             WHERE id = ?1 AND owner_epoch = ?4",
        )
        .bind(task_id.as_str())
        .bind(runtime.as_str())
        .bind(next.get() as i64)
        .bind(expected_epoch.get() as i64)
        .execute(self.pool())
        .await
        .map_err(StorageError::from)?;
        if updated.rows_affected() == 1 {
            return Ok(OwnershipToken {
                task_id: task_id.clone(),
                runtime_id: runtime.clone(),
                owner_epoch: next,
            });
        }
        match self.current(task_id).await? {
            Some(actual) => Err(OwnershipError::Stale {
                task_id: task_id.clone(),
                expected_epoch,
                actual_runtime: actual.runtime,
                actual_epoch: actual.epoch,
            }),
            None => Err(OwnershipError::UnknownTask {
                task_id: task_id.clone(),
            }),
        }
    }
}

/// The shared in-memory ownership authority. One instance is shared by the
/// memory ownership store AND every memory fenced store, and its mutex is
/// held across fenced check+write sections — the memory equivalent of the
/// SQLite transaction, so no observable half-state exists even under
/// concurrency.
#[derive(Default)]
pub struct MemoryOwnershipState {
    /// task_id → (owner runtime, epoch). Absent = task unknown here; fenced
    /// stores treat "no ownership row registered" as unowned epoch 0.
    owners: Mutex<HashMap<String, (Option<String>, u64)>>,
}

impl MemoryOwnershipState {
    /// An empty ownership authority.
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a task as existing (unowned, epoch 0) so `acquire` can find
    /// it — the memory analogue of the `tasks` row insert.
    pub fn register_task(&self, task_id: &TaskId) {
        self.owners
            .lock()
            .unwrap()
            .entry(task_id.as_str().to_string())
            .or_insert((None, 0));
    }

    /// Whether `token` is the task's current ownership. Used by memory fenced
    /// stores WHILE HOLDING their own row locks inside `with_current`.
    fn is_current_locked(
        owners: &HashMap<String, (Option<String>, u64)>,
        token: &OwnershipToken,
    ) -> bool {
        owners
            .get(token.task_id.as_str())
            .is_some_and(|(runtime, epoch)| {
                runtime.as_deref() == Some(token.runtime_id.as_str())
                    && *epoch == token.owner_epoch.get()
            })
    }

    /// Run `write` only if `token` is current, holding the ownership lock for
    /// the whole check+write — a concurrent CAS cannot interleave, so the
    /// fenced write is atomic with its check (same observable contract as the
    /// SQLite conditional statement).
    pub fn with_current<T>(
        &self,
        token: &OwnershipToken,
        write: impl FnOnce() -> T,
    ) -> Result<T, StorageError> {
        let owners = self.owners.lock().unwrap();
        if !Self::is_current_locked(&owners, token) {
            let (runtime, epoch) = owners
                .get(token.task_id.as_str())
                .cloned()
                .unwrap_or((None, 0));
            return Err(StorageError::InvalidData(format!(
                "stale ownership for task {}: token epoch {}, current owner {:?} at epoch {}",
                token.task_id, token.owner_epoch, runtime, epoch
            )));
        }
        Ok(write())
    }
}

/// An in-memory [`OwnershipStore`] over the shared state.
pub struct MemoryOwnershipStore {
    state: std::sync::Arc<MemoryOwnershipState>,
}

impl MemoryOwnershipStore {
    /// Wrap the shared ownership state.
    pub fn new(state: std::sync::Arc<MemoryOwnershipState>) -> Self {
        Self { state }
    }
}

#[async_trait]
impl OwnershipStore for MemoryOwnershipStore {
    async fn current(&self, task_id: &TaskId) -> Result<Option<TaskOwner>, StorageError> {
        Ok(self
            .state
            .owners
            .lock()
            .unwrap()
            .get(task_id.as_str())
            .map(|(runtime, epoch)| TaskOwner {
                runtime: runtime.clone().map(RuntimeId::new),
                epoch: OwnerEpoch::new(*epoch),
            }))
    }

    async fn acquire(
        &self,
        task_id: &TaskId,
        runtime: &RuntimeId,
        expected_epoch: OwnerEpoch,
    ) -> Result<OwnershipToken, OwnershipError> {
        let next = expected_epoch
            .next()
            .ok_or_else(|| OwnershipError::EpochExhausted {
                task_id: task_id.clone(),
            })?;
        let mut owners = self.state.owners.lock().unwrap();
        let Some(entry) = owners.get_mut(task_id.as_str()) else {
            return Err(OwnershipError::UnknownTask {
                task_id: task_id.clone(),
            });
        };
        if entry.1 != expected_epoch.get() {
            return Err(OwnershipError::Stale {
                task_id: task_id.clone(),
                expected_epoch,
                actual_runtime: entry.0.clone().map(RuntimeId::new),
                actual_epoch: OwnerEpoch::new(entry.1),
            });
        }
        *entry = (Some(runtime.as_str().to_string()), next.get());
        Ok(OwnershipToken {
            task_id: task_id.clone(),
            runtime_id: runtime.clone(),
            owner_epoch: next,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{SessionRecord, SessionRepository, TaskStore};
    use std::sync::Arc;

    /// Shared contract (Scenarios A/B/C/L + exhaustion) against both
    /// implementations, exercised through the port only.
    async fn assert_ownership_contract(store: &dyn OwnershipStore, task: &TaskId) {
        let a = leveler_core::RuntimeId::new("rt-a");
        let b = leveler_core::RuntimeId::new("rt-b");

        // Unknown task: typed error.
        assert!(matches!(
            store
                .acquire(&TaskId::new("ghost"), &a, OwnerEpoch::UNOWNED)
                .await,
            Err(OwnershipError::UnknownTask { .. })
        ));

        // Scenario A: initial acquire → epoch 1.
        let current = store.current(task).await.unwrap().unwrap();
        assert_eq!(
            current,
            TaskOwner {
                runtime: None,
                epoch: OwnerEpoch::UNOWNED
            }
        );
        let t1 = store.acquire(task, &a, OwnerEpoch::UNOWNED).await.unwrap();
        assert_eq!(t1.owner_epoch.get(), 1);

        // Scenario B: same runtime reacquire → epoch 2; old expectation stale.
        let t2 = store.acquire(task, &a, t1.owner_epoch).await.unwrap();
        assert_eq!(t2.owner_epoch.get(), 2);
        let stale = store.acquire(task, &a, t1.owner_epoch).await;
        assert!(
            matches!(stale, Err(OwnershipError::Stale { actual_epoch, .. }) if actual_epoch.get() == 2)
        );

        // Scenario C: CAS to another runtime → epoch 3; blind steal (wrong
        // expected epoch) refused.
        assert!(matches!(
            store.acquire(task, &b, OwnerEpoch::new(1)).await,
            Err(OwnershipError::Stale { .. }),
        ));
        let t3 = store.acquire(task, &b, t2.owner_epoch).await.unwrap();
        assert_eq!(t3.owner_epoch.get(), 3);
        assert_eq!(
            store.current(task).await.unwrap().unwrap().runtime.as_ref(),
            Some(&b)
        );

        // Epoch exhaustion fails loudly, never wraps.
        assert!(matches!(
            store.acquire(task, &a, OwnerEpoch::new(u64::MAX)).await,
            Err(OwnershipError::EpochExhausted { .. })
        ));
    }

    async fn sqlite_with_task() -> (Database, TaskId) {
        let db = Database::connect_in_memory().await.unwrap();
        let record = SessionRecord::new("/repo", "goal", "mock/m", leveler_core::now());
        SessionRepository::new(&db).create(&record).await.unwrap();
        let session = leveler_core::SessionId::new(record.id);
        let task = TaskStore::ensure_for_session(&db, &session, leveler_core::now())
            .await
            .unwrap();
        (db, task)
    }

    #[tokio::test]
    async fn sqlite_store_honors_the_contract() {
        let (db, task) = sqlite_with_task().await;
        assert_ownership_contract(&db, &task).await;
    }

    #[tokio::test]
    async fn memory_store_honors_the_contract() {
        let state = Arc::new(MemoryOwnershipState::new());
        let task = TaskId::new("task-1");
        state.register_task(&task);
        let store = MemoryOwnershipStore::new(state);
        assert_ownership_contract(&store, &task).await;
    }

    /// Scenario L: two contenders with the same expectation — exactly one
    /// wins, the epoch advances exactly once.
    #[tokio::test]
    async fn concurrent_acquisition_has_exactly_one_winner() {
        let (db, task) = sqlite_with_task().await;
        let db = Arc::new(db);
        let mut join = tokio::task::JoinSet::new();
        for i in 0..8 {
            let db = db.clone();
            let task = task.clone();
            join.spawn(async move {
                let rt = leveler_core::RuntimeId::new(format!("rt-{i}"));
                OwnershipStore::acquire(db.as_ref(), &task, &rt, OwnerEpoch::UNOWNED).await
            });
        }
        let mut winners = 0;
        let mut stale = 0;
        while let Some(result) = join.join_next().await {
            match result.unwrap() {
                Ok(token) => {
                    winners += 1;
                    assert_eq!(token.owner_epoch.get(), 1);
                }
                Err(OwnershipError::Stale { actual_epoch, .. }) => {
                    stale += 1;
                    assert_eq!(actual_epoch.get(), 1, "losers must see the winner's epoch");
                }
                Err(other) => panic!("unexpected acquire failure: {other}"),
            }
        }
        assert_eq!(
            winners, 1,
            "exactly one contender may win ({stale} refused)"
        );
        assert_eq!(
            OwnershipStore::current(db.as_ref(), &task)
                .await
                .unwrap()
                .unwrap()
                .epoch
                .get(),
            1,
            "the epoch advances exactly once"
        );
    }
}
