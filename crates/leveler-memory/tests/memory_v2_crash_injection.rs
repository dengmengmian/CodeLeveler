//! Real process-termination tests for the Markdown store's commit protocol.
//!
//! The parent test spawns this same test binary as a child. The child drives
//! one store operation and aborts at a named failpoint, leaving the on-disk
//! state exactly as a power loss would. The parent then reopens the store and
//! verifies what recovery produced.
//!
//! A child that exits normally never reached its failpoint, so every test
//! asserts the child died from SIGABRT. These tests are built only with the
//! `crash-injection` feature (see `[[test]]` in `Cargo.toml`), so a production
//! build has no failpoint code at all.

#![cfg(unix)]

mod memory_v2_common;

use std::os::unix::process::ExitStatusExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

use leveler_memory::v2::{
    AdmitRequest, Admission, Caller, Cas, ForgetOutcome, ListQuery, MarkdownStore, MemoryId,
    MemoryScope, MemoryStatus, OpId, RecordPatch, Relation, ScopeRoots, StoreError,
};
use memory_v2_common::*;

const CALLER_INVOCATION: &str = "invocation-crash-test";
const CALLER_TOOL_CALL: &str = "call-crash-test";
const USER_CALLER: &str = "user";
const MODEL_CALLER: &str = "model";

fn caller(kind: &str) -> Caller {
    match kind {
        MODEL_CALLER => Caller::Model {
            tool_call: CALLER_TOOL_CALL.to_string(),
        },
        _ => Caller::UserCommand {
            invocation: CALLER_INVOCATION.to_string(),
        },
    }
}

fn op(value: &str) -> OpId {
    OpId::parse(value.to_string()).unwrap()
}

fn cas_from_env() -> Cas {
    Cas {
        revision: std::env::var("V2_CRASH_REVISION")
            .unwrap()
            .parse()
            .unwrap(),
        digest: std::env::var("V2_CRASH_DIGEST").unwrap(),
    }
}

fn fact_body(value: &str) -> String {
    format!("The shared value is {value}.")
}

fn spawn(
    fixture: &Fixture,
    scenario: &str,
    crash_at: Option<&str>,
    env: &[(&str, String)],
) -> Output {
    spawn_at(&fixture.current, &fixture.state, scenario, crash_at, env)
}

fn spawn_at(
    current: &Path,
    state: &Path,
    scenario: &str,
    crash_at: Option<&str>,
    env: &[(&str, String)],
) -> Output {
    let executable = std::env::current_exe().unwrap();
    let mut command = Command::new(executable);
    command
        .args(["--exact", "crash_child_worker", "--ignored", "--nocapture"])
        .env("V2_CRASH_SCENARIO", scenario)
        .env("V2_CRASH_CURRENT", current.to_str().unwrap())
        .env("V2_CRASH_STATE", state.to_str().unwrap())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    match crash_at {
        Some(crash_at) => command.env("LEVELER_MEMORY_CRASH_AT", crash_at),
        None => command.env_remove("LEVELER_MEMORY_CRASH_AT"),
    };
    for (key, value) in env {
        command.env(key, value);
    }
    command.output().expect("spawn the crash child")
}

fn assert_aborted_at(output: &Output, crash_at: &str) {
    assert_eq!(
        output.status.signal(),
        Some(6),
        "child did not abort at {crash_at}; status={:?}\nstdout:\n{}\nstderr:\n{}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
}

fn files_in(dir: &Path) -> Vec<PathBuf> {
    match std::fs::read_dir(dir) {
        Ok(entries) => entries
            .filter_map(Result::ok)
            .map(|entry| entry.path())
            .collect(),
        Err(_) => Vec::new(),
    }
}

/// Every crash must leave recovery able to converge on one clean state.
fn assert_recovered_clean(store: &MarkdownStore) {
    let paths = store.paths();
    assert!(
        files_in(&paths.journal_dir).is_empty(),
        "recovery left a journal behind: {:?}",
        files_in(&paths.journal_dir)
    );
    assert!(
        files_in(&paths.stage_dir).is_empty(),
        "recovery left a staged file behind: {:?}",
        files_in(&paths.stage_dir)
    );
    let report = store.recover().unwrap();
    assert!(!report.is_blocked(), "{report:?}");
    assert!(report.recovered.is_empty(), "{report:?}");
}

fn seed_alpha(fixture: &Fixture) -> (MemoryId, Cas) {
    let store = fixture.store();
    let admission = create_user(
        &store,
        MemoryScope::Project,
        "shared_subject",
        "alpha",
        &fact_body("alpha"),
    );
    let id = applied_id(&admission);
    let read = store.read(&id).unwrap();
    (id, read.cas)
}

fn assert_single_value(store: &MarkdownStore, expected: &str) {
    let active = effective(store);
    assert_eq!(active.len(), 1, "{active:?}");
    assert_eq!(
        active[0].record.frontmatter.value.as_deref(),
        Some(expected),
        "{active:?}"
    );
}

// ----- child worker -------------------------------------------------------

#[test]
#[ignore]
fn crash_child_worker() {
    let Ok(scenario) = std::env::var("V2_CRASH_SCENARIO") else {
        return;
    };
    let current = std::env::var("V2_CRASH_CURRENT").unwrap();
    let state = std::env::var("V2_CRASH_STATE").unwrap();
    let store =
        MarkdownStore::open(ScopeRoots::new(MemoryScope::Project, current, state)).unwrap();
    let op = op(&std::env::var("V2_CRASH_OP").unwrap());
    let caller = caller(
        std::env::var("V2_CRASH_CALLER")
            .unwrap_or_else(|_| USER_CALLER.to_string())
            .as_str(),
    );
    let value = std::env::var("V2_CRASH_VALUE").unwrap();
    match scenario.as_str() {
        "create" => {
            let subject = std::env::var("V2_CRASH_SUBJECT").unwrap();
            let admission = store
                .admit(AdmitRequest {
                    op_id: op,
                    caller,
                    intent: fact_intent(
                        MemoryScope::Project,
                        Some(&subject),
                        &value,
                        &fact_body(&value),
                    ),
                    expected: None,
                })
                .unwrap();
            assert!(admission.is_applied(), "{admission:?}");
        }
        "update" => {
            let target = MemoryId::parse(std::env::var("V2_CRASH_TARGET").unwrap()).unwrap();
            let admission = store
                .update(
                    op,
                    caller,
                    &target,
                    RecordPatch {
                        value: Some(value.clone()),
                        body: Some(fact_body(&value)),
                        ..Default::default()
                    },
                    cas_from_env(),
                )
                .unwrap();
            assert!(
                admission.is_applied() || matches!(admission, Admission::Pending { .. }),
                "{admission:?}"
            );
        }
        "supersede" => {
            let target = MemoryId::parse(std::env::var("V2_CRASH_TARGET").unwrap()).unwrap();
            let subject = std::env::var("V2_CRASH_SUBJECT").unwrap();
            let mut intent = fact_intent(
                MemoryScope::Project,
                Some(&subject),
                &value,
                &fact_body(&value),
            );
            intent.relation = Relation::Supersede {
                targets: vec![target],
            };
            let admission = store
                .admit(AdmitRequest {
                    op_id: op,
                    caller,
                    intent,
                    expected: None,
                })
                .unwrap();
            assert!(admission.is_applied(), "{admission:?}");
        }
        "forget" => {
            let target = MemoryId::parse(std::env::var("V2_CRASH_TARGET").unwrap()).unwrap();
            let outcome = store.forget(op, caller, &target, cas_from_env()).unwrap();
            assert!(matches!(outcome, ForgetOutcome::Forgotten(_)), "{outcome:?}");
        }
        other => panic!("unknown crash scenario: {other}"),
    }
}

#[test]
#[ignore]
fn crash_recovery_reader_helper() {
    let (Ok(current), Ok(state)) = (
        std::env::var("V2_CRASH_CURRENT"),
        std::env::var("V2_CRASH_STATE"),
    ) else {
        return;
    };
    let store =
        MarkdownStore::open(ScopeRoots::new(MemoryScope::Project, current, state)).unwrap();
    let active = store.effective_active().unwrap();
    println!("ACTIVE={}", active.len());
    let pending = store.list_pending().unwrap();
    println!("PENDING={}", pending.len());
}

// ----- journal prepared ---------------------------------------------------

#[test]
fn crash_after_the_journal_is_prepared_leaves_one_current_record() {
    let fixture = Fixture::new(MemoryScope::Project);
    let _ = fixture.store();
    let op = op("op-crash-prepared");
    let output = spawn(
        &fixture,
        "create",
        Some("journal_prepared"),
        &[
            ("V2_CRASH_OP", op.to_string()),
            ("V2_CRASH_SUBJECT", "billing_model".into()),
            ("V2_CRASH_VALUE", "subscription".into()),
            ("V2_CRASH_CALLER", USER_CALLER.into()),
        ],
    );
    assert_aborted_at(&output, "journal_prepared");

    let store = fixture.store();
    assert_single_value(&store, "subscription");
    assert_recovered_clean(&store);
    let id = effective(&store)[0].record.frontmatter.id.clone();
    assert!(fixture.current_file(&id).exists());
    assert!(
        store.paths().history_file(&id, 1).exists(),
        "history for the committed revision is missing"
    );

    // The same operation must still replay as already applied.
    let replay = store
        .admit(AdmitRequest {
            op_id: op,
            caller: caller(USER_CALLER),
            intent: fact_intent(
                MemoryScope::Project,
                Some("billing_model"),
                "subscription",
                &fact_body("subscription"),
            ),
            expected: None,
        })
        .unwrap();
    assert!(matches!(replay, Admission::Replayed { .. }), "{replay:?}");
}

#[test]
fn concurrent_readers_recover_a_prepared_journal_exactly_once() {
    let fixture = Fixture::new(MemoryScope::Project);
    let _ = fixture.store();
    let output = spawn(
        &fixture,
        "create",
        Some("journal_prepared"),
        &[
            ("V2_CRASH_OP", "op-crash-lock-order".into()),
            ("V2_CRASH_SUBJECT", "billing_model".into()),
            ("V2_CRASH_VALUE", "subscription".into()),
            ("V2_CRASH_CALLER", USER_CALLER.into()),
        ],
    );
    assert_aborted_at(&output, "journal_prepared");

    // Every reader opens the store, which takes the exclusive lock and runs
    // recovery. If shared-then-exclusive lock upgrades deadlocked, these
    // children would hit the lock timeout instead of converging.
    let executable = std::env::current_exe().unwrap();
    let mut children = Vec::new();
    for _ in 0..4 {
        let child = Command::new(&executable)
            .args([
                "--exact",
                "crash_recovery_reader_helper",
                "--ignored",
                "--nocapture",
            ])
            .env("V2_CRASH_CURRENT", fixture.current.to_str().unwrap())
            .env("V2_CRASH_STATE", fixture.state.to_str().unwrap())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn a recovery reader");
        children.push(child);
    }
    for child in children {
        let output = child.wait_with_output().unwrap();
        assert!(
            output.status.success(),
            "recovery reader failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(stdout.contains("ACTIVE=1"), "{stdout}");
        assert!(stdout.contains("PENDING=0"), "{stdout}");
    }

    let store = fixture.store();
    assert_single_value(&store, "subscription");
    assert_recovered_clean(&store);
}

#[cfg(target_os = "macos")]
#[test]
fn crash_recovery_across_devices_converges() {
    let Some(volume) = MountedVolume::new("crash") else {
        eprintln!("hdiutil could not mount a volume; cross-device recovery NOT_VERIFIED");
        return;
    };
    let state_dir = tempfile::tempdir().unwrap();
    let current = volume.path().join("memory");
    let state = state_dir.path().join("state");
    let output = spawn_at(
        &current,
        &state,
        "create",
        Some("journal_prepared"),
        &[
            ("V2_CRASH_OP", "op-crash-cross-device".into()),
            ("V2_CRASH_SUBJECT", "billing_model".into()),
            ("V2_CRASH_VALUE", "subscription".into()),
            ("V2_CRASH_CALLER", USER_CALLER.into()),
        ],
    );
    assert_aborted_at(&output, "journal_prepared");

    let store = MarkdownStore::open(ScopeRoots::new(MemoryScope::Project, &current, &state)).unwrap();
    assert_single_value(&store, "subscription");
    assert_recovered_clean(&store);
}

// ----- current installed --------------------------------------------------

#[test]
fn crash_after_the_new_current_file_is_installed_finishes_the_transaction() {
    let fixture = Fixture::new(MemoryScope::Project);
    let _ = fixture.store();
    let output = spawn(
        &fixture,
        "create",
        Some("current_installed"),
        &[
            ("V2_CRASH_OP", "op-crash-installed".into()),
            ("V2_CRASH_SUBJECT", "billing_model".into()),
            ("V2_CRASH_VALUE", "subscription".into()),
            ("V2_CRASH_CALLER", USER_CALLER.into()),
        ],
    );
    assert_aborted_at(&output, "current_installed");

    let store = fixture.store();
    assert_single_value(&store, "subscription");
    assert_recovered_clean(&store);
}

// ----- old target retired -------------------------------------------------

#[test]
fn crash_after_the_old_target_is_retired_leaves_only_the_new_truth() {
    let fixture = Fixture::new(MemoryScope::Project);
    let (old_id, _) = seed_alpha(&fixture);
    let output = spawn(
        &fixture,
        "supersede",
        Some("targets_retired"),
        &[
            ("V2_CRASH_OP", "op-crash-retired".into()),
            ("V2_CRASH_SUBJECT", "shared_subject".into()),
            ("V2_CRASH_VALUE", "beta".into()),
            ("V2_CRASH_CALLER", USER_CALLER.into()),
            ("V2_CRASH_TARGET", old_id.to_string()),
        ],
    );
    assert_aborted_at(&output, "targets_retired");

    let store = fixture.store();
    assert_single_value(&store, "beta");
    assert_recovered_clean(&store);
    assert!(
        !fixture.current_file(&old_id).exists(),
        "the retired target came back as current"
    );
    assert!(
        store.paths().history_file(&old_id, 1).exists(),
        "the retired target lost its history"
    );
    let read = store.read(&old_id);
    assert!(
        matches!(read, Err(StoreError::NotCurrent { .. })),
        "retired target read: {read:?}"
    );
}

// ----- history and receipt writes ----------------------------------------

#[test]
fn crash_while_history_is_finalized_keeps_history_and_current_consistent() {
    let fixture = Fixture::new(MemoryScope::Project);
    let _ = fixture.store();
    let output = spawn(
        &fixture,
        "create",
        Some("history_finalized"),
        &[
            ("V2_CRASH_OP", "op-crash-history".into()),
            ("V2_CRASH_SUBJECT", "billing_model".into()),
            ("V2_CRASH_VALUE", "subscription".into()),
            ("V2_CRASH_CALLER", USER_CALLER.into()),
        ],
    );
    assert_aborted_at(&output, "history_finalized");

    let store = fixture.store();
    assert_single_value(&store, "subscription");
    assert_recovered_clean(&store);
    let id = effective(&store)[0].record.frontmatter.id.clone();
    let current = std::fs::read(fixture.current_file(&id)).unwrap();
    let history = std::fs::read(store.paths().history_file(&id, 1)).unwrap();
    assert_eq!(current, history, "history and current diverged");
}

#[test]
fn crash_while_the_receipt_is_finalized_is_recoverable() {
    let fixture = Fixture::new(MemoryScope::Project);
    let _ = fixture.store();
    let output = spawn(
        &fixture,
        "create",
        Some("receipt_finalized"),
        &[
            ("V2_CRASH_OP", "op-crash-receipt".into()),
            ("V2_CRASH_SUBJECT", "billing_model".into()),
            ("V2_CRASH_VALUE", "subscription".into()),
            ("V2_CRASH_CALLER", USER_CALLER.into()),
        ],
    );
    assert_aborted_at(&output, "receipt_finalized");

    let store = fixture.store();
    assert_single_value(&store, "subscription");
    assert_recovered_clean(&store);
    let id = effective(&store)[0].record.frontmatter.id.clone();
    assert!(
        store.paths().receipt_file(&id).exists(),
        "the receipt was lost across recovery"
    );
}

// ----- journal committed --------------------------------------------------

#[test]
fn crash_after_the_journal_is_committed_only_cleans_up() {
    let fixture = Fixture::new(MemoryScope::Project);
    let _ = fixture.store();
    let output = spawn(
        &fixture,
        "create",
        Some("journal_committed"),
        &[
            ("V2_CRASH_OP", "op-crash-committed".into()),
            ("V2_CRASH_SUBJECT", "billing_model".into()),
            ("V2_CRASH_VALUE", "subscription".into()),
            ("V2_CRASH_CALLER", USER_CALLER.into()),
        ],
    );
    assert_aborted_at(&output, "journal_committed");

    let store = fixture.store();
    assert_single_value(&store, "subscription");
    assert_recovered_clean(&store);
}

// ----- forget -------------------------------------------------------------

#[test]
fn crash_during_forget_deletes_the_body_and_keeps_a_tombstone() {
    let fixture = Fixture::new(MemoryScope::Project);
    let (id, cas) = seed_alpha(&fixture);
    let output = spawn(
        &fixture,
        "forget",
        Some("forget_current_removed"),
        &[
            ("V2_CRASH_OP", "op-crash-forget".into()),
            ("V2_CRASH_VALUE", "alpha".into()),
            ("V2_CRASH_CALLER", USER_CALLER.into()),
            ("V2_CRASH_TARGET", id.to_string()),
            ("V2_CRASH_REVISION", cas.revision.to_string()),
            ("V2_CRASH_DIGEST", cas.digest.clone()),
        ],
    );
    assert_aborted_at(&output, "forget_current_removed");

    let store = fixture.store();
    assert!(effective(&store).is_empty(), "a forgotten record is active");
    assert_recovered_clean(&store);
    assert!(!fixture.current_file(&id).exists());
    assert!(
        store.paths().tombstone_file(&id).exists(),
        "the tombstone was lost"
    );
    let read = store.read(&id);
    assert!(
        matches!(read, Err(StoreError::Forgotten(_))),
        "forgotten read: {read:?}"
    );
    // History and receipts may not keep a body copy after a forget.
    let history_dir = store.paths().history_dir_for(&id);
    for file in files_in(&history_dir) {
        let text = std::fs::read_to_string(&file).unwrap_or_default();
        assert!(
            !text.contains("alpha"),
            "forget left a body copy at {}",
            file.display()
        );
    }
}

// ----- pending ------------------------------------------------------------

#[test]
fn crash_while_parking_a_pending_proposal_never_activates_it() {
    let fixture = Fixture::new(MemoryScope::Project);
    let (id, cas) = seed_alpha(&fixture);
    let output = spawn(
        &fixture,
        "update",
        Some("journal_prepared"),
        &[
            ("V2_CRASH_OP", "op-crash-pending".into()),
            ("V2_CRASH_VALUE", "beta".into()),
            ("V2_CRASH_CALLER", MODEL_CALLER.into()),
            ("V2_CRASH_TARGET", id.to_string()),
            ("V2_CRASH_REVISION", cas.revision.to_string()),
            ("V2_CRASH_DIGEST", cas.digest.clone()),
        ],
    );
    assert_aborted_at(&output, "journal_prepared");

    let store = fixture.store();
    assert_single_value(&store, "alpha");
    assert_recovered_clean(&store);
    let pending = store.list_pending().unwrap();
    assert_eq!(pending.len(), 1, "{pending:?}");
    assert_eq!(pending[0].frontmatter.status, MemoryStatus::Pending);
    assert_eq!(pending[0].frontmatter.value.as_deref(), Some("beta"));
    assert_eq!(
        store
            .list(ListQuery {
                scope: None,
                kind: None,
                status: Some(MemoryStatus::Active),
                limit: None,
                include_external: true,
            })
            .unwrap()
            .len(),
        1,
        "the proposal became active without review"
    );
}
