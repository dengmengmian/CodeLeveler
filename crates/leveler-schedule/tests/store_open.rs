//! Opening one brand-new schedule store from several processes at once.
//!
//! The store is user-global, so the TUI, the CLI and a dispatcher pass can all
//! race to create `schedules.db`. SQLite serializes writes, but sqlx's
//! migrator checks the ledger in a deferred transaction: without a retry the
//! loser observes the winner's table and aborts. The store must converge on
//! one schema instead of failing an otherwise valid read.

use std::sync::Arc;

use leveler_schedule::ScheduleStore;
use tokio::sync::Barrier;

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_first_open_converges_on_one_schema() {
    for round in 0..10 {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("state").join("schedules.db");
        let barrier = Arc::new(Barrier::new(4));
        let mut tasks = Vec::new();
        for _ in 0..4 {
            let path = path.clone();
            let barrier = barrier.clone();
            tasks.push(tokio::spawn(async move {
                barrier.wait().await;
                ScheduleStore::open(&path).await.map(|_| ())
            }));
        }
        for (index, task) in tasks.into_iter().enumerate() {
            task.await
                .expect("task panicked")
                .unwrap_or_else(|error| panic!("round {round}, opener {index}: {error}"));
        }
        assert!(path.exists(), "round {round}: the store was not created");
    }
}
