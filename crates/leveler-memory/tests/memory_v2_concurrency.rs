//! Stage 1 concurrency and reader consistency.

mod memory_v2_common;

use std::process::{Command, Stdio};
use std::sync::{Barrier, Mutex};
use std::thread;

use leveler_memory::v2::{Admission, AdmitRequest, MarkdownStore, MemoryScope, ScopeRoots};
use memory_v2_common::*;

#[test]
fn concurrent_writers_of_distinct_subjects_lose_no_updates() {
    let fixture = Fixture::new(MemoryScope::Project);
    let _ = fixture.store();
    let roots = fixture.roots();
    thread::scope(|scope| {
        for worker in 0..4 {
            let roots = roots.clone();
            scope.spawn(move || {
                let store = MarkdownStore::open(roots).unwrap();
                for index in 0..5 {
                    let key = format!("worker_{worker}_fact_{index}");
                    let body = format!("Fact {worker}-{index}.");
                    let admission = create_user(&store, MemoryScope::Project, &key, "value", &body);
                    assert!(admission.is_applied(), "{admission:?}");
                }
            });
        }
    });
    let store = fixture.store();
    assert_eq!(effective(&store).len(), 20);
}

#[test]
fn concurrent_writers_of_one_subject_leave_one_current_value() {
    let fixture = Fixture::new(MemoryScope::Project);
    let _ = fixture.store();
    let roots = fixture.roots();
    let barrier = Barrier::new(6);
    thread::scope(|scope| {
        for worker in 0..6 {
            let roots = roots.clone();
            let barrier = &barrier;
            scope.spawn(move || {
                let store = MarkdownStore::open(roots).unwrap();
                barrier.wait();
                let admission = create_user(
                    &store,
                    MemoryScope::Project,
                    "shared_subject",
                    &format!("value-{worker}"),
                    &format!("The shared value is value-{worker}."),
                );
                assert!(admission.is_applied(), "{admission:?}");
            });
        }
    });
    let store = fixture.store();
    let active = effective(&store);
    assert_eq!(active.len(), 1);
    assert!(
        active[0]
            .record
            .frontmatter
            .value
            .as_deref()
            .is_some_and(|value| value.starts_with("value-"))
    );
    let history_dirs = std::fs::read_dir(&store.paths().history_dir)
        .unwrap()
        .filter_map(Result::ok)
        .filter(|entry| entry.file_type().map(|kind| kind.is_dir()).unwrap_or(false))
        .count();
    assert_eq!(
        history_dirs, 6,
        "every created revision, including the current one, has history"
    );
}

#[test]
fn the_same_operation_racing_twice_applies_once_and_replays_once() {
    let fixture = Fixture::new(MemoryScope::Project);
    let _ = fixture.store();
    let roots = fixture.roots();
    let request = AdmitRequest {
        op_id: op(),
        caller: user(),
        intent: fact_intent(
            MemoryScope::Project,
            Some("billing_model"),
            "subscription",
            "The provider bills by subscription.",
        ),
        expected: None,
    };
    let results = Mutex::new(Vec::new());
    let barrier = Barrier::new(2);
    thread::scope(|scope| {
        for _ in 0..2 {
            let roots = roots.clone();
            let request = request.clone();
            let results = &results;
            let barrier = &barrier;
            scope.spawn(move || {
                let store = MarkdownStore::open(roots).unwrap();
                barrier.wait();
                let admission = store.admit(request).unwrap();
                results.lock().unwrap().push(admission);
            });
        }
    });
    let results = results.into_inner().unwrap();
    assert_eq!(
        results
            .iter()
            .filter(|admission| matches!(admission, Admission::Applied { .. }))
            .count(),
        1,
        "{results:?}"
    );
    assert_eq!(
        results
            .iter()
            .filter(|admission| matches!(admission, Admission::Replayed { .. }))
            .count(),
        1,
        "{results:?}"
    );
    assert_eq!(effective(&fixture.store()).len(), 1);
}

#[test]
fn readers_never_observe_zero_or_two_active_records_during_supersedes() {
    let fixture = Fixture::new(MemoryScope::Project);
    let store = fixture.store();
    create_user(
        &store,
        MemoryScope::Project,
        "billing_model",
        "plan-0",
        "The provider bills by plan 0.",
    );
    drop(store);
    let roots = fixture.roots();
    let barrier = Barrier::new(4);
    thread::scope(|scope| {
        {
            let roots = roots.clone();
            let barrier = &barrier;
            scope.spawn(move || {
                let store = MarkdownStore::open(roots).unwrap();
                barrier.wait();
                for plan in 1..=40 {
                    let admission = create_user(
                        &store,
                        MemoryScope::Project,
                        "billing_model",
                        &format!("plan-{plan}"),
                        &format!("The provider bills by plan {plan}."),
                    );
                    assert!(admission.is_applied(), "{admission:?}");
                }
            });
        }
        for _ in 0..3 {
            let roots = roots.clone();
            let barrier = &barrier;
            scope.spawn(move || {
                let store = MarkdownStore::open(roots).unwrap();
                barrier.wait();
                for _ in 0..200 {
                    let active = store.effective_active().unwrap();
                    assert_eq!(active.len(), 1, "only one complete truth may be visible");
                    let body = &active[0].record.body;
                    let plan = body
                        .strip_prefix("The provider bills by plan ")
                        .and_then(|value| value.strip_suffix('.'))
                        .and_then(|value| value.parse::<u32>().ok());
                    assert!(plan.is_some(), "a reader saw a partial body: {body:?}");
                }
            });
        }
    });
    let store = fixture.store();
    assert_eq!(effective(&store).len(), 1);
}

#[test]
#[ignore]
fn child_writer_helper() {
    let (Ok(current), Ok(state), Ok(subject), Ok(value)) = (
        std::env::var("V2_CHILD_CURRENT"),
        std::env::var("V2_CHILD_STATE"),
        std::env::var("V2_CHILD_SUBJECT"),
        std::env::var("V2_CHILD_VALUE"),
    ) else {
        return;
    };
    let roots = ScopeRoots::new(MemoryScope::Project, current, state);
    let store = MarkdownStore::open(roots).unwrap();
    let admission = store
        .admit(AdmitRequest {
            op_id: op(),
            caller: user(),
            intent: fact_intent(
                MemoryScope::Project,
                Some(&subject),
                &value,
                &format!("The shared value is {value}."),
            ),
            expected: None,
        })
        .unwrap();
    assert!(admission.is_applied(), "{admission:?}");
}

#[test]
fn cross_process_writers_serialize_and_leave_one_current_value() {
    let fixture = Fixture::new(MemoryScope::Project);
    let _ = fixture.store();
    let current = fixture.current.to_str().unwrap().to_string();
    let state = fixture.state.to_str().unwrap().to_string();
    let executable = std::env::current_exe().unwrap();
    let mut children = Vec::new();
    for worker in 0..4 {
        let child = Command::new(&executable)
            .args(["--exact", "child_writer_helper", "--ignored", "--nocapture"])
            .env("V2_CHILD_CURRENT", &current)
            .env("V2_CHILD_STATE", &state)
            .env("V2_CHILD_SUBJECT", "shared_subject")
            .env("V2_CHILD_VALUE", format!("child-{worker}"))
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn the cross-process writer");
        children.push(child);
    }
    for child in children {
        let output = child.wait_with_output().unwrap();
        assert!(
            output.status.success(),
            "child writer failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    let store = fixture.store();
    let active = effective(&store);
    assert_eq!(active.len(), 1);
    assert!(
        active[0]
            .record
            .frontmatter
            .value
            .as_deref()
            .is_some_and(|value| value.starts_with("child-"))
    );
    let history_dirs = std::fs::read_dir(&store.paths().history_dir)
        .unwrap()
        .filter_map(Result::ok)
        .filter(|entry| entry.file_type().map(|kind| kind.is_dir()).unwrap_or(false))
        .count();
    assert_eq!(
        history_dirs, 4,
        "every created revision, including the current one, has history"
    );
}
