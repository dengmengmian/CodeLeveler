//! Concurrent first callers must share one database initialization.
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};
use tracing::{
    Event, Metadata, Subscriber,
    field::{Field, Visit},
    span::{Attributes, Id, Record},
};

struct InitializationQueries(Arc<AtomicUsize>);
struct ForeignKeyCheck(bool);
impl Visit for ForeignKeyCheck {
    fn record_debug(&mut self, _field: &Field, value: &dyn std::fmt::Debug) {
        self.0 |= format!("{value:?}").contains("PRAGMA foreign_key_check");
    }
}
impl Subscriber for InitializationQueries {
    fn enabled(&self, _: &Metadata<'_>) -> bool {
        true
    }
    fn new_span(&self, _: &Attributes<'_>) -> Id {
        Id::from_u64(1)
    }
    fn record(&self, _: &Id, _: &Record<'_>) {}
    fn record_follows_from(&self, _: &Id, _: &Id) {}
    fn event(&self, event: &Event<'_>) {
        if event.metadata().target() != "sqlx::query" {
            return;
        }
        let mut check = ForeignKeyCheck(false);
        event.record(&mut check);
        if check.0 {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
    }
    fn enter(&self, _: &Id) {}
    fn exit(&self, _: &Id) {}
}

#[tokio::test]
async fn concurrent_first_opens_initialize_the_database_once() {
    let count = Arc::new(AtomicUsize::new(0));
    tracing::subscriber::set_global_default(InitializationQueries(count.clone())).unwrap();
    let tmp = tempfile::tempdir().unwrap();
    let layout = leveler_project::Layout::from_parts(
        tmp.path().join("repo"),
        tmp.path().join("configs"),
        tmp.path().join("state"),
    );
    let app = leveler_app::Application::assemble(layout).unwrap();
    let results = futures::future::join_all((0..8).map(|_| app.open_database())).await;
    for result in results {
        result.unwrap();
    }
    assert_eq!(
        count.load(Ordering::SeqCst),
        1,
        "one pool initialization, including its migration integrity check"
    );
}
