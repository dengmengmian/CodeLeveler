//! C1/C3/C4: real SIGKILL around the `/compact` epoch cut.
//!
//! The child is this test binary, run with `--exact epoch_cut_child`. It uses
//! a real file SQLite, a real cut transaction, two consumed inputs, a tool
//! exchange and a durable delivery. The failpoint writes a marker and SIGKILLs
//! itself; the parent reads the durable state afterwards and runs the reaper.

#![cfg(unix)]

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use leveler_core::{BootId, OwnerEpoch, RuntimeId, SessionId, TaskId, Timestamp, TurnId};
use leveler_storage::{
    Database, EventRepository, MessageRepository, MessageStore, OwnershipStore,
    SessionInputRepository, SessionRecord, SessionRepository, TaskStore, TurnRepository,
};

fn now() -> Timestamp {
    leveler_core::now()
}

fn user_payload(text: &str) -> String {
    serde_json::json!({
        "role": "user",
        "content": [{"type": "text", "text": text}]
    })
    .to_string()
}

/// Child entry. Runs only when the parent set `EPOCH_CUT_DB`; otherwise a
/// normal `cargo test` run returns immediately.
#[tokio::test]
async fn epoch_cut_child() {
    let Ok(path) = std::env::var("EPOCH_CUT_DB") else {
        return;
    };
    let db = Database::connect(Path::new(&path)).await.unwrap();
    let record = SessionRecord::new("/repo", "goal", "mock/m", now());
    let session = SessionId::new(record.id.clone());
    SessionRepository::new(&db).create(&record).await.unwrap();
    let task = TaskStore::ensure_for_session(&db, &session, now())
        .await
        .unwrap();
    let token = OwnershipStore::acquire(
        &db,
        &task,
        &RuntimeId::new("rt-crash"),
        &BootId::new("boot-crash"),
        OwnerEpoch::UNOWNED,
    )
    .await
    .unwrap();

    let mut turn_ids = Vec::new();
    for (id, text) in [("in-1", "first task"), ("in-2", "second task")] {
        let turn = TurnRepository::new(&db)
            .start(&session, "user", None, now())
            .await
            .unwrap();
        let turn_id = TurnId::new(turn.id);
        SessionInputRepository::new(&db)
            .admit(id, &session, Some(&turn_id), text, now())
            .await
            .unwrap();
        db.append_input_in_turn_owned(&token, &session, &turn_id, id, &user_payload(text), now())
            .await
            .unwrap();
        turn_ids.push(turn_id);
    }

    // A completed tool exchange and a durable delivery must survive the cut.
    let events = EventRepository::new(&db);
    events
        .append(
            &session,
            None,
            "tool_call_started",
            r#"{"call":"c1"}"#,
            now(),
        )
        .await
        .unwrap();
    events
        .append(
            &session,
            None,
            "tool_call_finished",
            r#"{"call":"c1"}"#,
            now(),
        )
        .await
        .unwrap();
    events
        .append(
            &session,
            None,
            "delivery_committed",
            r#"{"delivery":1}"#,
            now(),
        )
        .await
        .unwrap();

    let state_path = std::env::var("EPOCH_CUT_STATE").unwrap();
    std::fs::write(
        &state_path,
        serde_json::json!({
            "session": session.as_str(),
            "task": task.as_str(),
            "epoch": token.owner_epoch.get(),
            "turns": turn_ids.iter().map(|id| id.as_str()).collect::<Vec<_>>(),
        })
        .to_string(),
    )
    .unwrap();

    let summary = r#"{"role":"user","content":[{"type":"text","text":"compacted summary"}]}"#;
    let epoch_events = vec![("compacted".to_string(), r#"{"from":2,"to":1}"#.to_string())];
    db.cut_context_epoch(&session, &[summary.to_string()], &epoch_events, now())
        .await
        .unwrap();

    // Only reached when the failpoint was not armed (normal run).
    let _ = (turn_ids, token);
}

fn run_child(point: &str) -> (tempfile::TempDir, PathBuf, std::process::ExitStatus) {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("session.db");
    let marker = dir.path().join("hit.marker");
    let exe = std::env::current_exe().unwrap();
    let mut child = Command::new(exe)
        .arg("epoch_cut_child")
        .arg("--exact")
        .arg("--nocapture")
        .env("EPOCH_CUT_DB", &db_path)
        .env("EPOCH_CUT_STATE", dir.path().join("state.json"))
        .env("LEVELER_TEST_CRASH_POINT", point)
        .env("LEVELER_TEST_CRASH_MARKER", &marker)
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(20);
    while !marker.exists() {
        assert!(
            Instant::now() < deadline,
            "failpoint {point} was not reached; child still running"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
    let status = child.wait().unwrap();
    (dir, db_path, status)
}

#[tokio::test]
async fn c1_sigkill_before_the_cut_leaves_the_original_transcript() {
    let (dir, path, status) = run_child("before_context_epoch_cut");
    use std::os::unix::process::ExitStatusExt;
    assert_eq!(
        status.signal(),
        Some(9),
        "child must die by SIGKILL: {status:?}"
    );

    let db = Database::connect(&path).await.unwrap();
    let state: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(dir.path().join("state.json")).unwrap())
            .unwrap();
    let session = SessionId::new(state["session"].as_str().unwrap());
    let messages = MessageRepository::new(&db).load(&session).await.unwrap();
    assert_eq!(
        messages.len(),
        2,
        "both initiating rows survive: {messages:?}"
    );
    let journal = SessionInputRepository::new(&db)
        .all(&session)
        .await
        .unwrap();
    assert_eq!(
        journal
            .iter()
            .filter(|i| i.state.as_str() == "consumed")
            .count(),
        2
    );
    let compacted = EventRepository::new(&db)
        .load_last_by_type(&session, "compacted", None)
        .await
        .unwrap();
    assert!(compacted.is_none(), "the cut must not be durable");
    drop(dir);
}

#[tokio::test]
async fn c3_sigkill_after_the_cut_does_not_reproject_consumed_inputs() {
    let (dir, path, status) = run_child("after_context_epoch_cut");
    use std::os::unix::process::ExitStatusExt;
    assert_eq!(
        status.signal(),
        Some(9),
        "child must die by SIGKILL: {status:?}"
    );

    let db = Database::connect(&path).await.unwrap();
    let state: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(dir.path().join("state.json")).unwrap())
            .unwrap();
    let session = SessionId::new(state["session"].as_str().unwrap());
    let messages = MessageRepository::new(&db).load(&session).await.unwrap();
    assert_eq!(messages.len(), 1, "summary-only epoch: {messages:?}");
    assert!(messages[0].contains("compacted summary"));
    let journal = SessionInputRepository::new(&db)
        .all(&session)
        .await
        .unwrap();
    assert_eq!(
        journal
            .iter()
            .filter(|i| i.state.as_str() == "consumed")
            .count(),
        2,
        "both input consumption facts survive: {journal:?}"
    );
    let events = EventRepository::new(&db).load(&session).await.unwrap();
    assert_eq!(
        events
            .iter()
            .filter(|e| e.event_type == "tool_call_started")
            .count(),
        1
    );
    assert_eq!(
        events
            .iter()
            .filter(|e| e.event_type == "delivery_committed")
            .count(),
        1
    );
    assert_eq!(
        events
            .iter()
            .filter(|e| e.event_type == "compacted")
            .count(),
        1
    );

    let task = TaskId::new(state["task"].as_str().unwrap());
    let token = OwnershipStore::acquire(
        &db,
        &task,
        &RuntimeId::new("rt-reaper"),
        &BootId::new("boot-reaper"),
        OwnerEpoch::new(state["epoch"].as_u64().unwrap()),
    )
    .await
    .unwrap();
    let turn_id = TurnId::new(state["turns"][1].as_str().unwrap());
    let inserted = db
        .ensure_initiating_message_owned(
            &token,
            &session,
            &turn_id,
            &user_payload("second task"),
            now(),
        )
        .await
        .unwrap();
    assert!(!inserted, "the reaper must not resurrect a consumed input");
    let again = db
        .ensure_initiating_message_owned(
            &token,
            &session,
            &turn_id,
            &user_payload("second task"),
            now(),
        )
        .await
        .unwrap();
    assert!(!again, "a repeated reaper is idempotent");
    assert_eq!(
        MessageRepository::new(&db)
            .load(&session)
            .await
            .unwrap()
            .len(),
        1,
        "no duplicate initiating message"
    );
    drop(dir);
}
