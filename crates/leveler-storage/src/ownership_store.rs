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

use leveler_core::{BootId, OwnerEpoch, OwnershipToken, RuntimeId, TaskId};

use crate::{Database, StorageError};

/// A task's current ownership state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TaskOwner {
    /// The owning runtime; `None` while unowned (epoch history retained).
    pub runtime: Option<RuntimeId>,
    /// The boot that acquired the current epoch; `None` while unowned, and on
    /// ownership recorded before boots were.
    pub boot: Option<BootId>,
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
    /// `expected_epoch`, atomically set `runtime` and `boot` as owner at
    /// `expected_epoch + 1` and return the new token. Whether `boot` may take
    /// the task from its current owner is the caller's decision; this is only
    /// the atomic write. Any mismatch is
    /// [`OwnershipError::Stale`] with the actual state. Exactly one of two
    /// concurrent callers with the same expectation wins.
    async fn acquire(
        &self,
        task_id: &TaskId,
        runtime: &RuntimeId,
        boot: &BootId,
        expected_epoch: OwnerEpoch,
    ) -> Result<OwnershipToken, OwnershipError>;
}

/// The production SQLite adapter over the `tasks` ownership columns
/// (migration 0017). The CAS is one conditional UPDATE — atomic by
/// construction, no check-then-write window.
#[async_trait]
impl OwnershipStore for Database {
    async fn current(&self, task_id: &TaskId) -> Result<Option<TaskOwner>, StorageError> {
        let row: Option<(Option<String>, Option<String>, i64)> = sqlx::query_as(
            "SELECT owner_runtime_id, owner_boot_id, owner_epoch FROM tasks WHERE id = ?1",
        )
        .bind(task_id.as_str())
        .fetch_optional(self.pool())
        .await?;
        Ok(row.map(|(runtime, boot, epoch)| TaskOwner {
            runtime: runtime.map(RuntimeId::new),
            boot: boot.map(BootId::new),
            epoch: OwnerEpoch::new(epoch.max(0) as u64),
        }))
    }

    async fn acquire(
        &self,
        task_id: &TaskId,
        runtime: &RuntimeId,
        boot: &BootId,
        expected_epoch: OwnerEpoch,
    ) -> Result<OwnershipToken, OwnershipError> {
        let next = expected_epoch
            .next()
            .ok_or_else(|| OwnershipError::EpochExhausted {
                task_id: task_id.clone(),
            })?;
        let updated = sqlx::query(
            "UPDATE tasks SET owner_runtime_id = ?2, owner_boot_id = ?5, owner_epoch = ?3 \
             WHERE id = ?1 AND owner_epoch = ?4",
        )
        .bind(task_id.as_str())
        .bind(runtime.as_str())
        .bind(next.get() as i64)
        .bind(expected_epoch.get() as i64)
        .bind(boot.as_str())
        .execute(self.pool())
        .await
        .map_err(StorageError::from)?;
        if updated.rows_affected() == 1 {
            return Ok(OwnershipToken {
                task_id: task_id.clone(),
                runtime_id: runtime.clone(),
                boot_id: boot.clone(),
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

type MemoryOwner = (Option<String>, Option<String>, u64);

/// The shared in-memory ownership authority. One instance is shared by the
/// memory ownership store AND every memory fenced store, and its mutex is
/// held across fenced check+write sections — the memory equivalent of the
/// SQLite transaction, so no observable half-state exists even under
/// concurrency.
#[derive(Default)]
pub struct MemoryOwnershipState {
    /// task_id → (owner runtime, owner boot, epoch). Absent = task unknown
    /// here; fenced stores treat "no ownership row registered" as unowned
    /// epoch 0.
    owners: Mutex<HashMap<String, MemoryOwner>>,
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
            .or_insert((None, None, 0));
    }

    /// Whether `token` is the task's current ownership. Used by memory fenced
    /// stores WHILE HOLDING their own row locks inside `with_current`.
    fn is_current_locked(owners: &HashMap<String, MemoryOwner>, token: &OwnershipToken) -> bool {
        owners
            .get(token.task_id.as_str())
            .is_some_and(|(runtime, _, epoch)| {
                runtime.as_deref() == Some(token.runtime_id.as_str())
                    && *epoch == token.owner_epoch.get()
            })
    }

    /// Run `write` only if `token` is current, holding the ownership lock for
    /// the whole check+write — a concurrent CAS cannot interleave, so the
    /// fenced write is atomic with its check (same observable contract as the
    /// SQLite conditional statement). Lock order everywhere: ownership first,
    /// then the store's own row lock inside `write`.
    pub fn with_current<T>(
        &self,
        token: &OwnershipToken,
        write: impl FnOnce() -> T,
    ) -> Result<T, OwnershipError> {
        let owners = self.owners.lock().unwrap();
        if !Self::is_current_locked(&owners, token) {
            return Err(Self::stale_locked(&owners, token));
        }
        Ok(write())
    }

    /// The task-terminal section: `commit` runs while `token` is current, and
    /// an inserted terminal releases the task in the same critical section,
    /// keeping its epoch. Once released, the same generation may run `commit`
    /// again only when its terminal is `recorded` — a replay, never a second
    /// terminal; any later generation makes the token stale.
    pub(crate) fn finish_task(
        &self,
        token: &OwnershipToken,
        recorded: impl FnOnce() -> bool,
        commit: impl FnOnce() -> Result<crate::TaskTerminalCommit, StorageError>,
    ) -> Result<crate::TaskTerminalCommit, OwnershipError> {
        let mut owners = self.owners.lock().unwrap();
        let current = Self::is_current_locked(&owners, token);
        let released = owners
            .get(token.task_id.as_str())
            .is_some_and(|(runtime, _, epoch)| {
                runtime.is_none() && *epoch == token.owner_epoch.get()
            });
        if !(current || released && recorded()) {
            return Err(Self::stale_locked(&owners, token));
        }
        let commit = commit()?;
        if commit.inserted {
            owners.insert(
                token.task_id.as_str().to_string(),
                (None, None, token.owner_epoch.get()),
            );
        }
        Ok(commit)
    }

    fn stale_locked(
        owners: &HashMap<String, MemoryOwner>,
        token: &OwnershipToken,
    ) -> OwnershipError {
        let (runtime, _, epoch) = owners
            .get(token.task_id.as_str())
            .cloned()
            .unwrap_or((None, None, 0));
        OwnershipError::Stale {
            task_id: token.task_id.clone(),
            expected_epoch: token.owner_epoch,
            actual_runtime: runtime.map(RuntimeId::new),
            actual_epoch: OwnerEpoch::new(epoch),
        }
    }
}

/// SQLite-side helper: build the typed stale error for a fenced write that
/// matched zero rows, reading the task's actual owner state.
pub(crate) async fn sqlite_stale_error(db: &Database, token: &OwnershipToken) -> OwnershipError {
    match OwnershipStore::current(db, &token.task_id).await {
        Ok(Some(actual)) => OwnershipError::Stale {
            task_id: token.task_id.clone(),
            expected_epoch: token.owner_epoch,
            actual_runtime: actual.runtime,
            actual_epoch: actual.epoch,
        },
        Ok(None) => OwnershipError::UnknownTask {
            task_id: token.task_id.clone(),
        },
        Err(error) => OwnershipError::Storage(error),
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
            .map(|(runtime, boot, epoch)| TaskOwner {
                runtime: runtime.clone().map(RuntimeId::new),
                boot: boot.clone().map(BootId::new),
                epoch: OwnerEpoch::new(*epoch),
            }))
    }

    async fn acquire(
        &self,
        task_id: &TaskId,
        runtime: &RuntimeId,
        boot: &BootId,
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
        if entry.2 != expected_epoch.get() {
            return Err(OwnershipError::Stale {
                task_id: task_id.clone(),
                expected_epoch,
                actual_runtime: entry.0.clone().map(RuntimeId::new),
                actual_epoch: OwnerEpoch::new(entry.2),
            });
        }
        *entry = (
            Some(runtime.as_str().to_string()),
            Some(boot.as_str().to_string()),
            next.get(),
        );
        Ok(OwnershipToken {
            task_id: task_id.clone(),
            runtime_id: runtime.clone(),
            boot_id: boot.clone(),
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
        let boot_1 = BootId::new("boot-1");
        let boot_2 = BootId::new("boot-2");

        // Unknown task: typed error.
        assert!(matches!(
            store
                .acquire(&TaskId::new("ghost"), &a, &boot_1, OwnerEpoch::UNOWNED)
                .await,
            Err(OwnershipError::UnknownTask { .. })
        ));

        // Scenario A: initial acquire → epoch 1, owned by the acquiring boot.
        let current = store.current(task).await.unwrap().unwrap();
        assert_eq!(
            current,
            TaskOwner {
                runtime: None,
                boot: None,
                epoch: OwnerEpoch::UNOWNED
            }
        );
        let t1 = store
            .acquire(task, &a, &boot_1, OwnerEpoch::UNOWNED)
            .await
            .unwrap();
        assert_eq!(t1.owner_epoch.get(), 1);
        assert_eq!(t1.boot_id, boot_1);
        assert_eq!(
            store.current(task).await.unwrap().unwrap(),
            TaskOwner {
                runtime: Some(a.clone()),
                boot: Some(boot_1.clone()),
                epoch: t1.owner_epoch
            }
        );

        // Scenario B: same runtime reacquire → epoch 2; old expectation stale.
        let t2 = store
            .acquire(task, &a, &boot_1, t1.owner_epoch)
            .await
            .unwrap();
        assert_eq!(t2.owner_epoch.get(), 2);
        let stale = store.acquire(task, &a, &boot_2, t1.owner_epoch).await;
        assert!(
            matches!(stale, Err(OwnershipError::Stale { actual_epoch, .. }) if actual_epoch.get() == 2)
        );
        assert_eq!(
            store.current(task).await.unwrap().unwrap().boot,
            Some(boot_1.clone()),
            "a refused acquire leaves the owning boot in place"
        );

        // Scenario C: CAS to another runtime → epoch 3; blind steal (wrong
        // expected epoch) refused.
        assert!(matches!(
            store.acquire(task, &b, &boot_2, OwnerEpoch::new(1)).await,
            Err(OwnershipError::Stale { .. }),
        ));
        let t3 = store
            .acquire(task, &b, &boot_2, t2.owner_epoch)
            .await
            .unwrap();
        assert_eq!(t3.owner_epoch.get(), 3);
        assert_eq!(
            store.current(task).await.unwrap().unwrap(),
            TaskOwner {
                runtime: Some(b.clone()),
                boot: Some(boot_2.clone()),
                epoch: t3.owner_epoch
            },
            "runtime, boot and epoch change together"
        );

        // Epoch exhaustion fails loudly, never wraps.
        assert!(matches!(
            store
                .acquire(task, &a, &boot_1, OwnerEpoch::new(u64::MAX))
                .await,
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
                let boot = BootId::new(format!("boot-{i}"));
                OwnershipStore::acquire(db.as_ref(), &task, &rt, &boot, OwnerEpoch::UNOWNED).await
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
