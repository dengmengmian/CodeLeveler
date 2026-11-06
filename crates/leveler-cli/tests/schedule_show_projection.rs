//! `leveler schedule show` must project the same next run every other surface
//! sees.
//!
//! The store keeps every occurrence, including terminal ones. "Next run" is a
//! pending-occurrence question; reading an arbitrary row (the newest by time)
//! makes a finished one-off look scheduled again. These tests prepare real
//! occurrence states through the store, then run the CLI as a separate process
//! against the same home and compare its projection with the shared
//! `ScheduleManager` view the TUI and the agent tools render.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use chrono::{DateTime, Utc};
use leveler_client_protocol::schedule::{ScheduleRequest, ScheduleResponse};
use leveler_schedule::{
    DefinitionUpdate, OccurrenceState, RunOutcome, ScheduleId, ScheduleManager, ScheduleStore,
};

fn scratch(name: &str) -> PathBuf {
    static SEQ: AtomicU64 = AtomicU64::new(1);
    let n = SEQ.fetch_add(1, Ordering::Relaxed);
    let path = PathBuf::from(format!("/tmp/lvl-show-{}-{n}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&path);
    std::fs::create_dir_all(&path).unwrap();
    path
}

fn leveler(home: &Path, workspace: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_leveler"))
        .args(args)
        .current_dir(workspace)
        .env("HOME", home)
        .env("LEVELER_HOME", home.join("leveler"))
        .env("LEVELER_LANG", "en")
        .env("NO_COLOR", "1")
        .env_remove("LEVELER_CONFIG_DIR")
        .env_remove("DEEPSEEK_API_KEY")
        .env_remove("LEVELER_API_KEY")
        .output()
        .unwrap_or_else(|error| panic!("could not run leveler {args:?}: {error}"))
}

fn json(output: &Output) -> serde_json::Value {
    assert!(
        output.status.success(),
        "command failed: {}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).unwrap_or_else(|error| {
        panic!(
            "not JSON ({error}): {}",
            String::from_utf8_lossy(&output.stdout)
        )
    })
}

fn workspace(home: &Path) -> PathBuf {
    let workspace = home.join("ws");
    std::fs::create_dir_all(workspace.join(".leveler")).unwrap();
    std::fs::write(workspace.join(".leveler").join("config.yaml"), "{}\n").unwrap();
    std::fs::canonicalize(workspace).unwrap()
}

/// Stored timestamps keep milliseconds; use the same precision so a value
/// written by the CLI and a value read back compare equal.
fn future(offset_secs: i64) -> (DateTime<Utc>, String) {
    let millis = Utc::now().timestamp_millis() + offset_secs * 1000;
    let at = DateTime::from_timestamp_millis(millis).expect("timestamp");
    (at, at.to_rfc3339())
}

fn projected(json: &serde_json::Value) -> Option<DateTime<Utc>> {
    json["next_occurrence"]
        .as_str()
        .map(|value| value.parse().expect("an RFC3339 instant"))
}

async fn store(home: &Path) -> ScheduleStore {
    ScheduleStore::open(&home.join("leveler").join("state").join("schedules.db"))
        .await
        .expect("the CLI's store")
}

async fn manager(home: &Path, store: &ScheduleStore) -> ScheduleManager {
    ScheduleManager::new(Arc::new(store.clone()), home.join("leveler"), None)
}

async fn shared_next(view: &ScheduleManager, id: &str) -> Option<DateTime<Utc>> {
    match view.handle(ScheduleRequest::Show { id: id.into() }).await {
        ScheduleResponse::Show { schedule } => schedule
            .next_run_at
            .as_deref()
            .map(|value| value.parse().expect("a projected instant")),
        other => panic!("manager show failed: {other:?}"),
    }
}

async fn add_once(home: &Path, workspace: &Path, at: &str) -> (String, serde_json::Value) {
    let workspace_text = workspace.display().to_string();
    let created = json(&leveler(
        home,
        workspace,
        &[
            "schedule",
            "add",
            "--workspace",
            &workspace_text,
            "--name",
            "one off",
            "--at",
            at,
            "--command",
            "printf once",
            "--json",
        ],
    ));
    let id = created["id"].as_str().unwrap().to_string();
    (id, created)
}

/// Claim the definition's one pending slot and settle it with `outcome`.
async fn settle(store: &ScheduleStore, id: &str, outcome: RunOutcome) {
    let id = ScheduleId::from(id);
    let occurrence = store
        .next_pending_occurrence(&id)
        .await
        .unwrap()
        .expect("the fixture must have a pending slot");
    let claim = store
        .claim_due(
            occurrence.scheduled_for + chrono::Duration::seconds(1),
            Duration::from_secs(60),
            10,
        )
        .await
        .unwrap()
        .into_iter()
        .find(|claim| claim.definition.id == id)
        .expect("the pending slot must be claimable");
    store.finish_run(&claim.run_id, outcome).await.unwrap();
}

#[tokio::test]
async fn a_new_one_off_projects_its_declared_instant() {
    let home = scratch("new-once");
    let workspace = workspace(&home);
    let (t1, t1_text) = future(3_600);
    let (id, created) = add_once(&home, &workspace, &t1_text).await;
    assert_eq!(projected(&created), Some(t1));

    let shown = json(&leveler(
        &home,
        &workspace,
        &["schedule", "show", &id, "--json"],
    ));
    assert_eq!(projected(&shown), Some(t1));
}

#[tokio::test]
async fn an_updated_one_off_projects_only_the_new_instant() {
    let home = scratch("updated-once");
    let workspace = workspace(&home);
    let (_, t1_text) = future(3_600);
    let (id, _) = add_once(&home, &workspace, &t1_text).await;

    let (t2, t2_text) = future(7_200);
    let updated = json(&leveler(
        &home,
        &workspace,
        &["schedule", "update", &id, "--at", &t2_text, "--json"],
    ));
    assert_eq!(projected(&updated), Some(t2));

    let shown = json(&leveler(
        &home,
        &workspace,
        &["schedule", "show", &id, "--json"],
    ));
    assert_eq!(projected(&shown), Some(t2));
}

#[tokio::test]
async fn a_completed_one_off_projects_no_next_run() {
    let home = scratch("completed-once");
    let workspace = workspace(&home);
    let (_, t1_text) = future(3_600);
    let (id, _) = add_once(&home, &workspace, &t1_text).await;

    let store = store(&home).await;
    settle(&store, &id, RunOutcome::Succeeded { exit_code: Some(0) }).await;
    let manager = manager(&home, &store).await;
    assert_eq!(
        shared_next(&manager, &id).await,
        None,
        "the shared view must not project a finished one-off"
    );

    let shown = json(&leveler(
        &home,
        &workspace,
        &["schedule", "show", &id, "--json"],
    ));
    assert_eq!(
        projected(&shown),
        None,
        "a completed one-off has no next run; the old slot is history: {shown}"
    );
    assert_eq!(
        shown["recent_runs"][0]["state"], "succeeded",
        "the finished run stays visible as history"
    );

    let text = leveler(&home, &workspace, &["schedule", "show", &id]);
    assert!(text.status.success());
    let stdout = String::from_utf8_lossy(&text.stdout);
    assert!(
        stdout.contains("next:       none"),
        "the text surface must agree: {stdout}"
    );
}

#[tokio::test]
async fn a_cancelled_one_off_projects_no_next_run() {
    let home = scratch("cancelled-once");
    let workspace = workspace(&home);
    let (_, at) = future(3_600);
    let (id, _) = add_once(&home, &workspace, &at).await;

    let store = store(&home).await;
    settle(&store, &id, RunOutcome::Cancelled { error: None }).await;

    let shown = json(&leveler(
        &home,
        &workspace,
        &["schedule", "show", &id, "--json"],
    ));
    assert_eq!(projected(&shown), None, "cancelled is terminal: {shown}");
}

#[tokio::test]
async fn a_missed_one_off_projects_no_next_run() {
    let home = scratch("missed-once");
    let workspace = workspace(&home);
    let (_, past) = future(-3_600);
    let (id, created) = add_once(&home, &workspace, &past).await;
    assert_eq!(
        created["next_occurrence"].as_str().map(|_| ()),
        Some(()),
        "a newly declared past one-off is due now, not absent"
    );

    let store = store(&home).await;
    assert_eq!(
        store.reconcile_misfires(Utc::now()).await.unwrap(),
        1,
        "the skip policy settles the overdue slot as missed"
    );
    let occurrence = store
        .occurrences(&ScheduleId::from(id.as_str()), 10)
        .await
        .unwrap();
    assert_eq!(occurrence[0].state, OccurrenceState::Missed);

    let shown = json(&leveler(
        &home,
        &workspace,
        &["schedule", "show", &id, "--json"],
    ));
    assert_eq!(projected(&shown), None, "missed is terminal: {shown}");
}

#[tokio::test]
async fn a_recurring_schedule_projects_its_next_instant() {
    let home = scratch("recurring");
    let workspace = workspace(&home);
    let workspace_text = workspace.display().to_string();
    let (anchor, anchor_text) = future(3_600);
    let created = json(&leveler(
        &home,
        &workspace,
        &[
            "schedule",
            "add",
            "--workspace",
            &workspace_text,
            "--name",
            "interval",
            "--every",
            "3600",
            "--anchor",
            &anchor_text,
            "--command",
            "printf tick",
            "--json",
        ],
    ));
    let id = created["id"].as_str().unwrap().to_string();
    assert_eq!(projected(&created), Some(anchor));

    let store = store(&home).await;
    let manager = manager(&home, &store).await;
    let shared = shared_next(&manager, &id)
        .await
        .expect("the manager projects it");
    assert_eq!(shared, anchor);

    let shown = json(&leveler(
        &home,
        &workspace,
        &["schedule", "show", &id, "--json"],
    ));
    assert_eq!(
        projected(&shown),
        Some(shared),
        "CLI and the shared manager view must name the same instant"
    );
}

#[tokio::test]
async fn a_paused_definition_projects_no_generated_next_run_like_the_shared_view() {
    let home = scratch("paused");
    let workspace = workspace(&home);
    let (_, at) = future(3_600);
    let (id, _) = add_once(&home, &workspace, &at).await;
    assert!(
        leveler(&home, &workspace, &["schedule", "pause", &id])
            .status
            .success()
    );

    let store = store(&home).await;
    let manager = manager(&home, &store).await;
    assert_eq!(
        shared_next(&manager, &id).await,
        None,
        "the shared projection hides a paused definition's generated slot"
    );
    let shown = json(&leveler(
        &home,
        &workspace,
        &["schedule", "show", &id, "--json"],
    ));
    assert_eq!(
        projected(&shown),
        None,
        "the CLI must not report a slot the dispatcher would not claim: {shown}"
    );
}

#[tokio::test]
async fn the_cli_and_the_shared_view_agree_after_an_explicit_reschedule() {
    let home = scratch("rescheduled");
    let workspace = workspace(&home);
    let (_, first) = future(3_600);
    let (id, _) = add_once(&home, &workspace, &first).await;
    let store = store(&home).await;
    settle(&store, &id, RunOutcome::Succeeded { exit_code: Some(0) }).await;

    let (t2, t2_text) = future(7_200);
    let definition = store
        .definition(&ScheduleId::from(id.as_str()))
        .await
        .unwrap()
        .unwrap();
    store
        .update_definition(
            &definition.id,
            DefinitionUpdate {
                name: definition.name.clone(),
                workspace: definition.workspace.clone(),
                action: definition.action.clone(),
                trigger: leveler_schedule::ScheduleTrigger::Once {
                    at: t2_text.parse().unwrap(),
                },
                session_policy: definition.session_policy,
                overlap: definition.overlap,
                mode: definition.mode,
                model: definition.model.clone(),
                timeout: definition.timeout,
                misfire: definition.misfire,
                misfire_grace: definition.misfire_grace,
                grant: None,
            },
            None,
        )
        .await
        .unwrap();

    let manager = manager(&home, &store).await;
    let shared = shared_next(&manager, &id)
        .await
        .expect("explicitly rescheduled");
    assert_eq!(shared, t2);
    let shown = json(&leveler(
        &home,
        &workspace,
        &["schedule", "show", &id, "--json"],
    ));
    assert_eq!(projected(&shown), Some(shared));
}
