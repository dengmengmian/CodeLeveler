//! Which goals still owe work, and whether anybody is driving them.
//!
//! Long-goal P2. Read-only by construction: this module answers a question and
//! mutates nothing. A goal is never marked interrupted here — there is no such
//! state, deliberately. Ownership already records whether a live runtime holds
//! the task, and a second writer for that fact is what P1 refused to add.
//!
//! The rule mirrors the reaper's exactly, because the reaper is the component
//! that already decided what a runtime may speak about:
//!
//! ```text
//! same runtime, or unowned  →  ours to report
//! a different runtime       →  reported as foreign, never claimed
//! ```
//!
//! A join lives here rather than in `GoalStore` because the stores are narrow
//! ports by design; composing three of them is the caller's job.

use leveler_core::{RuntimeId, SessionId};
use leveler_storage::{EngineStores, GoalRecord, GoalState, StorageError};

/// One goal that still owes work, with the facts needed to judge it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnfinishedGoal {
    pub goal: GoalRecord,
    /// The conversation the goal ran in, so a user can go read it.
    pub session_id: SessionId,
    /// The runtime that last held the task, if any.
    pub owner: Option<RuntimeId>,
    /// Whether this runtime may speak about the goal at all.
    ///
    /// `false` means another runtime holds the task. It is still reported —
    /// silently omitting work that exists is worse than naming work we cannot
    /// act on — but it is never described as needing *this* user's attention.
    pub ours: bool,
    /// A turn is still marked running for this session.
    ///
    /// After the restart reaper has run, this is false for everything that was
    /// abandoned, so a `true` here means the goal is genuinely being driven
    /// right now and is not unfinished work at all.
    pub driving: bool,
}

impl UnfinishedGoal {
    /// Work that is owed, that this runtime may act on, and that nobody is
    /// currently doing. The only claim the UI is allowed to make.
    pub fn needs_attention(&self) -> bool {
        self.ours && !self.driving
    }
}

/// Every goal still owing work, newest first.
///
/// Call after the restart reaper: turn state must already be settled, or a
/// goal abandoned by a dead process still looks like it is being driven.
pub async fn list_unfinished_goals(
    stores: &EngineStores,
    runtime: &RuntimeId,
) -> Result<Vec<UnfinishedGoal>, StorageError> {
    let mut out = Vec::new();
    for goal in stores.goals.unfinished().await? {
        debug_assert_eq!(goal.state, GoalState::Running);
        let Some(session_id) = stores.tasks.session_for_task(&goal.task_id).await? else {
            // A goal whose task vanished cannot be acted on and cannot be
            // explained. Skipping is right; the cascade means this should not
            // happen, and inventing a session id would be worse.
            continue;
        };
        let owner = stores
            .ownership
            .current(&goal.task_id)
            .await?
            .and_then(|o| o.runtime);
        let ours = match owner.as_ref() {
            None => true,
            Some(id) => id == runtime,
        };
        let driving = session_is_running(stores, &session_id).await?;
        out.push(UnfinishedGoal {
            goal,
            session_id,
            owner,
            ours,
            driving,
        });
    }
    Ok(out)
}

async fn session_is_running(
    stores: &EngineStores,
    session_id: &SessionId,
) -> Result<bool, StorageError> {
    Ok(stores
        .turns
        .list_running(Some(session_id))
        .await
        .map(|running| !running.is_empty())
        .unwrap_or(false))
}

#[cfg(test)]
mod tests {
    use super::*;
    use leveler_core::{OwnerEpoch, TaskId};
    use leveler_storage::{
        Database, GoalStore, OwnershipStore, SessionRecord, SessionRepository, TaskStore,
    };

    async fn seed(db: &Database) -> (SessionId, TaskId) {
        let record = SessionRecord::new("/repo", "goal", "mock/m", leveler_core::now());
        SessionRepository::new(db).create(&record).await.unwrap();
        let session = SessionId::new(record.id);
        let task = db
            .ensure_for_session(&session, leveler_core::now())
            .await
            .unwrap();
        (session, task)
    }

    fn rt(id: &str) -> RuntimeId {
        RuntimeId::new(id)
    }

    #[tokio::test]
    async fn a_goal_nobody_owns_is_ours_to_report() {
        let db = Database::connect_in_memory().await.unwrap();
        let stores = EngineStores::from_database(&db);
        let (session, task) = seed(&db).await;
        db.open(&task, "unowned work", leveler_core::now())
            .await
            .unwrap();

        let found = list_unfinished_goals(&stores, &rt("rt-1")).await.unwrap();
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].session_id, session);
        assert_eq!(found[0].owner, None);
        assert!(found[0].ours);
        assert!(found[0].needs_attention());
    }

    /// RuntimeId survives a process restart, so a goal abandoned by the
    /// previous incarnation comes back owned by us. That is the ordinary
    /// crash case and it must read as ours.
    #[tokio::test]
    async fn a_goal_owned_by_this_runtime_is_ours() {
        let db = Database::connect_in_memory().await.unwrap();
        let stores = EngineStores::from_database(&db);
        let (_, task) = seed(&db).await;
        db.acquire(&task, &rt("rt-1"), OwnerEpoch::new(0))
            .await
            .unwrap();
        db.open(&task, "our work", leveler_core::now())
            .await
            .unwrap();

        let found = list_unfinished_goals(&stores, &rt("rt-1")).await.unwrap();
        assert!(found[0].ours);
        assert!(found[0].needs_attention());
    }

    /// The reaper never touches another runtime's task; neither does this.
    /// It is still reported — omitting work that exists is worse than naming
    /// work we cannot act on.
    #[tokio::test]
    async fn a_foreign_owned_goal_is_reported_but_never_claimed() {
        let db = Database::connect_in_memory().await.unwrap();
        let stores = EngineStores::from_database(&db);
        let (_, task) = seed(&db).await;
        db.acquire(&task, &rt("rt-other"), OwnerEpoch::new(0))
            .await
            .unwrap();
        db.open(&task, "someone else's work", leveler_core::now())
            .await
            .unwrap();

        let found = list_unfinished_goals(&stores, &rt("rt-1")).await.unwrap();
        assert_eq!(found.len(), 1, "still reported");
        assert_eq!(found[0].owner, Some(rt("rt-other")));
        assert!(!found[0].ours);
        assert!(
            !found[0].needs_attention(),
            "we cannot act on it, so we must not ask the user to"
        );
    }

    #[tokio::test]
    async fn a_settled_goal_is_not_unfinished_work() {
        let db = Database::connect_in_memory().await.unwrap();
        let stores = EngineStores::from_database(&db);
        let (_, task) = seed(&db).await;
        let goal = db
            .open(&task, "finished work", leveler_core::now())
            .await
            .unwrap();
        db.settle(&goal, leveler_core::now()).await.unwrap();

        assert!(
            list_unfinished_goals(&stores, &rt("rt-1"))
                .await
                .unwrap()
                .is_empty()
        );
    }

    /// Only the unfinished one shows up when both exist.
    #[tokio::test]
    async fn a_completed_goal_does_not_hide_or_join_the_unfinished_one() {
        let db = Database::connect_in_memory().await.unwrap();
        let stores = EngineStores::from_database(&db);
        let (_, task_a) = seed(&db).await;
        let (_, task_b) = seed(&db).await;

        let done = db
            .open(&task_a, "goal B", leveler_core::now())
            .await
            .unwrap();
        db.settle(&done, leveler_core::now()).await.unwrap();
        db.open(&task_b, "goal A", leveler_core::now())
            .await
            .unwrap();

        let found = list_unfinished_goals(&stores, &rt("rt-1")).await.unwrap();
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].goal.objective, "goal A");
    }

    /// Discovery is a read. Calling it must not advance, settle or drive
    /// anything — the boundary P1 asserted, re-asserted at the layer a UI
    /// actually calls.
    #[tokio::test]
    async fn listing_unfinished_goals_changes_nothing() {
        let db = Database::connect_in_memory().await.unwrap();
        let stores = EngineStores::from_database(&db);
        let (_, task) = seed(&db).await;
        let goal = db
            .open(&task, "untouched", leveler_core::now())
            .await
            .unwrap();
        let before = db.get(&goal).await.unwrap().unwrap();

        for _ in 0..3 {
            let _ = list_unfinished_goals(&stores, &rt("rt-1")).await.unwrap();
        }

        assert_eq!(
            db.get(&goal).await.unwrap().unwrap(),
            before,
            "discovery must not resume, settle or advance the goal"
        );
    }
}
