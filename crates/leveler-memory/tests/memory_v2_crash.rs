//! Stage 1 crash recovery: a prepared journal is the only authority that lets
//! recovery finish a transaction, and a third state must block every reader.

mod memory_v2_common;

use std::fs;
use std::path::PathBuf;

use chrono::Utc;
use leveler_memory::v2::journal::{
    AtticEntry, JournalPlan, JournalState, NewFilePlan, StateFileKind, StateFilePlan,
    TargetDisposition, TargetPlan,
};
use leveler_memory::v2::{
    Admission, IntentRecord, MarkdownStore, MemoryId, MemoryRecord, MemoryScope, MemoryStatus,
    OpId, Operation, Receipt, StoreError, Tombstone, TrustSummary, frontmatter, path, sha256_hex,
    sha256_hex_text,
};
use memory_v2_common::*;

struct CrashSetup {
    op: OpId,
    new_id: MemoryId,
    new_final: PathBuf,
    old_id: MemoryId,
    old_record: MemoryRecord,
    staged: PathBuf,
    state_files: Vec<StateFilePlan>,
}

fn state_tag(kind: &StateFileKind) -> &'static str {
    match kind {
        StateFileKind::History { .. } => "history",
        StateFileKind::Receipt => "receipt",
        StateFileKind::Intent => "intent",
        StateFileKind::Pending { .. } => "pending",
        StateFileKind::Attic { .. } => "attic",
        StateFileKind::Tombstone { .. } => "tombstone",
    }
}

fn state_temp(op: &OpId, kind: StateFileKind, final_path: PathBuf, bytes: &[u8]) -> StateFilePlan {
    let parent = final_path.parent().expect("state file parent");
    fs::create_dir_all(parent).unwrap();
    let name = final_path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("state");
    let temp = parent.join(format!(
        ".tmp-{}-{}-{name}",
        &sha256_hex_text(op.as_str())[..16],
        state_tag(&kind)
    ));
    let _ = fs::remove_file(&temp);
    path::write_new(&temp, bytes, 0o600).unwrap();
    StateFilePlan {
        kind,
        temp_path: temp,
        final_path,
        digest: sha256_hex(bytes),
    }
}

/// Build an interrupted user supersede: the new current file is staged, the
/// journal is prepared, and no current file has been touched yet.
fn prepare_supersede(fixture: &Fixture, store: &MarkdownStore) -> CrashSetup {
    let op = op();
    let old_id = applied_id(&create_user(
        store,
        MemoryScope::Project,
        "billing_model",
        "subscription",
        "The provider bills by subscription.",
    ));
    let old_bytes = fs::read(fixture.current_file(&old_id)).unwrap();
    let old_record = frontmatter::parse(&old_bytes).unwrap();

    let mut new_record = old_record.clone();
    new_record.frontmatter.id = MemoryId::new();
    new_record.frontmatter.revision = 1;
    new_record.frontmatter.value = Some("usage".into());
    new_record.frontmatter.supersedes = vec![old_id.to_string()];
    new_record.frontmatter.updated_at = Utc::now();
    new_record.body = "The provider bills by usage.".into();
    let new_bytes = frontmatter::render(&new_record).unwrap();
    let new_id = new_record.frontmatter.id.clone();
    let new_final = store.paths().current_file(&new_id);
    let staged = store.paths().stage_dir.join(format!(
        "{}-{new_id}.md.stage",
        &sha256_hex_text(op.as_str())[..16]
    ));
    let _ = fs::remove_file(&staged);
    path::write_new(&staged, &new_bytes, 0o600).unwrap();

    let history = state_temp(
        &op,
        StateFileKind::History {
            id: new_id.clone(),
            revision: 1,
        },
        store.paths().history_file(&new_id, 1),
        &new_bytes,
    );
    let receipt = Receipt::from_record(&new_record, &sha256_hex(&new_bytes));
    let receipt_plan = state_temp(
        &op,
        StateFileKind::Receipt,
        store.paths().receipt_file(&new_id),
        &receipt.to_json().unwrap(),
    );
    let intent_record = IntentRecord {
        op_id: op.clone(),
        request_digest: "crash-fixture".into(),
        admission: Some(Admission::Applied {
            id: new_id.clone(),
            revision: 1,
            operation: Operation::Supersede,
            possible_duplicate: Vec::new(),
        }),
        created_at: Utc::now().to_rfc3339(),
    };
    let intent_plan = state_temp(
        &op,
        StateFileKind::Intent,
        store.paths().intent_file(&op),
        &serde_json::to_vec(&intent_record).unwrap(),
    );
    let attic_entry = AtticEntry {
        id: old_id.clone(),
        kind: old_record.frontmatter.kind,
        scope: old_record.frontmatter.scope,
        revision: old_record.frontmatter.revision,
        digest: sha256_hex(&old_bytes),
        status: MemoryStatus::Superseded,
        superseded_by: Some(new_id.clone()),
        reason: None,
        updated_at: Utc::now().to_rfc3339(),
    };
    let attic_plan = state_temp(
        &op,
        StateFileKind::Attic { id: old_id.clone() },
        store.paths().attic_file(&old_id),
        &serde_json::to_vec(&attic_entry).unwrap(),
    );

    let mut journal = JournalPlan::new(op.clone(), Operation::Supersede, "user_command");
    journal.new_files = vec![NewFilePlan {
        id: new_id.clone(),
        revision: 1,
        digest: sha256_hex(&new_bytes),
        staged_path: staged.clone(),
        final_path: new_final.clone(),
    }];
    journal.targets = vec![TargetPlan {
        id: old_id.clone(),
        revision: old_record.frontmatter.revision,
        digest: sha256_hex(&old_bytes),
        disposition: TargetDisposition::Retire,
        final_status: Some(MemoryStatus::Superseded),
        superseded_by: Some(new_id.clone()),
    }];
    journal.state_files = vec![history, receipt_plan, intent_plan, attic_plan];
    journal.receipt = Some(receipt);
    journal.intent_record = Some(intent_record);
    journal.attic = vec![attic_entry];
    journal.write(&store.paths().journal_file(&op)).unwrap();

    let state_files = journal.state_files.clone();
    CrashSetup {
        op,
        new_id,
        new_final,
        old_id,
        old_record,
        staged,
        state_files,
    }
}

fn install_new(setup: &CrashSetup) {
    fs::rename(&setup.staged, &setup.new_final).unwrap();
}

fn retire_old(setup: &CrashSetup, fixture: &Fixture, store: &MarkdownStore) {
    let history = store
        .paths()
        .history_file(&setup.old_id, setup.old_record.frontmatter.revision);
    fs::create_dir_all(history.parent().unwrap()).unwrap();
    fs::copy(fixture.current_file(&setup.old_id), &history).unwrap();
    fs::remove_file(fixture.current_file(&setup.old_id)).unwrap();
}

fn finalize_state(setup: &CrashSetup) {
    for file in &setup.state_files {
        fs::rename(&file.temp_path, &file.final_path).unwrap();
    }
}

fn assert_supersede_committed(fixture: &Fixture, store: &MarkdownStore, setup: &CrashSetup) {
    let active = effective(store);
    assert_eq!(active.len(), 1, "exactly one current record must remain");
    assert_eq!(active[0].record.frontmatter.id, setup.new_id);
    assert_eq!(active[0].record.frontmatter.value.as_deref(), Some("usage"));
    let read = store.read(&setup.new_id).unwrap();
    assert_eq!(read.trust, TrustSummary::Trusted);
    assert!(matches!(
        store.read(&setup.old_id),
        Err(StoreError::NotCurrent {
            status: MemoryStatus::Superseded,
            ..
        })
    ));
    let old_history = store
        .history(&setup.old_id, setup.old_record.frontmatter.revision)
        .unwrap();
    assert_eq!(old_history.body, "The provider bills by subscription.");
    assert_eq!(
        store.history(&setup.new_id, 1).unwrap().body,
        "The provider bills by usage."
    );
    assert!(store.paths().receipt_file(&setup.new_id).exists());
    assert!(!store.paths().journal_file(&setup.op).exists());
    assert!(
        fs::read_dir(&store.paths().stage_dir)
            .unwrap()
            .next()
            .is_none(),
        "successful recovery must clean the stage directory"
    );
    assert!(!fixture.current_file(&setup.old_id).exists());
}

#[test]
fn a_prepared_supersede_is_completed_by_recovery() {
    let fixture = Fixture::new(MemoryScope::Project);
    let store = fixture.store();
    let setup = prepare_supersede(&fixture, &store);
    let report = store.recover().unwrap();
    assert!(report.recovered.contains(&setup.op));
    assert!(report.blocked.is_empty());
    assert_supersede_committed(&fixture, &store, &setup);
}

#[test]
fn recovery_finishes_after_the_new_file_was_installed() {
    let fixture = Fixture::new(MemoryScope::Project);
    let store = fixture.store();
    let setup = prepare_supersede(&fixture, &store);
    install_new(&setup);
    let report = store.recover().unwrap();
    assert!(report.recovered.contains(&setup.op));
    assert_supersede_committed(&fixture, &store, &setup);
}

#[test]
fn recovery_finishes_after_the_old_file_was_retired() {
    let fixture = Fixture::new(MemoryScope::Project);
    let store = fixture.store();
    let setup = prepare_supersede(&fixture, &store);
    install_new(&setup);
    retire_old(&setup, &fixture, &store);
    let report = store.recover().unwrap();
    assert!(report.recovered.contains(&setup.op));
    assert_supersede_committed(&fixture, &store, &setup);
}

#[test]
fn recovery_is_idempotent_when_all_files_were_already_finalized() {
    let fixture = Fixture::new(MemoryScope::Project);
    let store = fixture.store();
    let setup = prepare_supersede(&fixture, &store);
    install_new(&setup);
    retire_old(&setup, &fixture, &store);
    finalize_state(&setup);
    let report = store.recover().unwrap();
    assert!(report.recovered.contains(&setup.op));
    assert!(report.blocked.is_empty());
    assert_supersede_committed(&fixture, &store, &setup);
    let second = store.recover().unwrap();
    assert!(second.recovered.is_empty());
}

#[test]
fn a_third_state_blocks_readers_instead_of_exposing_a_partial_supersede() {
    let fixture = Fixture::new(MemoryScope::Project);
    let store = fixture.store();
    let setup = prepare_supersede(&fixture, &store);
    let third_state = format!(
        "{}\nAn external writer changed this record.\n",
        fs::read_to_string(fixture.current_file(&setup.old_id)).unwrap()
    );
    fs::write(fixture.current_file(&setup.old_id), third_state).unwrap();

    let report = store.recover().unwrap();
    assert!(report.blocked.contains(&setup.op), "{report:?}");
    let plan = JournalPlan::load(&store.paths().journal_file(&setup.op)).unwrap();
    assert_eq!(plan.state, JournalState::Blocked);
    assert!(matches!(
        store.read(&setup.new_id),
        Err(StoreError::RecoveryConflict { .. })
    ));
    assert!(matches!(
        store.effective_active(),
        Err(StoreError::RecoveryConflict { .. })
    ));
    let write = store.admit(leveler_memory::v2::AdmitRequest {
        op_id: op(),
        caller: user(),
        intent: fact_intent(
            MemoryScope::Project,
            Some("other_subject"),
            "value",
            "Another fact.",
        ),
        expected: None,
    });
    assert!(
        matches!(write, Err(StoreError::RecoveryConflict { .. })),
        "{write:?}"
    );
    let after = store.recover().unwrap();
    assert!(
        after.blocked.contains(&setup.op),
        "blocking is not self-healing"
    );
}

struct ForgetSetup {
    op: OpId,
    id: MemoryId,
    record: MemoryRecord,
    tombstone_plan: StateFilePlan,
}

fn prepare_forget(fixture: &Fixture, store: &MarkdownStore) -> ForgetSetup {
    let op = op();
    let id = applied_id(&create_user(
        store,
        MemoryScope::Project,
        "account_handle",
        "ACME-123",
        "The account handle is ACME-123.",
    ));
    let bytes = fs::read(fixture.current_file(&id)).unwrap();
    let record = frontmatter::parse(&bytes).unwrap();
    let tombstone = Tombstone {
        id: id.clone(),
        kind: record.frontmatter.kind,
        scope: record.frontmatter.scope,
        subject_key_sha256: record
            .frontmatter
            .subject_key
            .as_deref()
            .map(sha256_hex_text),
        last_revision: record.frontmatter.revision,
        content_sha256: sha256_hex(&bytes),
        forgotten_at: Utc::now().to_rfc3339(),
    };
    let tombstone_bytes = serde_json::to_vec(&tombstone).unwrap();
    let tombstone_plan = state_temp(
        &op,
        StateFileKind::Tombstone { id: id.clone() },
        store.paths().tombstone_file(&id),
        &tombstone_bytes,
    );
    let intent_record = IntentRecord {
        op_id: op.clone(),
        request_digest: "forget-fixture".into(),
        admission: Some(Admission::Applied {
            id: id.clone(),
            revision: record.frontmatter.revision,
            operation: Operation::Forget,
            possible_duplicate: Vec::new(),
        }),
        created_at: Utc::now().to_rfc3339(),
    };
    let intent_plan = state_temp(
        &op,
        StateFileKind::Intent,
        store.paths().intent_file(&op),
        &serde_json::to_vec(&intent_record).unwrap(),
    );
    let mut journal = JournalPlan::new(op.clone(), Operation::Forget, "user_command");
    journal.targets = vec![TargetPlan {
        id: id.clone(),
        revision: record.frontmatter.revision,
        digest: sha256_hex(&bytes),
        disposition: TargetDisposition::Delete,
        final_status: None,
        superseded_by: None,
    }];
    journal.state_files = vec![tombstone_plan.clone(), intent_plan];
    journal.intent_record = Some(intent_record);
    journal.tombstones = vec![tombstone.clone()];
    journal.write(&store.paths().journal_file(&op)).unwrap();
    ForgetSetup {
        op,
        id,
        record,
        tombstone_plan,
    }
}

#[test]
fn forget_recovery_cannot_resurrect_the_record() {
    let fixture = Fixture::new(MemoryScope::Project);
    let store = fixture.store();
    let setup = prepare_forget(&fixture, &store);
    let report = store.recover().unwrap();
    assert!(report.recovered.contains(&setup.op));
    assert!(matches!(
        store.read(&setup.id),
        Err(StoreError::Forgotten(_))
    ));
    assert!(matches!(
        store.history(&setup.id, setup.record.frontmatter.revision),
        Err(StoreError::Forgotten(_))
    ));
    assert!(effective(&store).is_empty());
    assert!(!fixture.current_file(&setup.id).exists());
    assert!(!store.paths().history_dir_for(&setup.id).exists());
    assert!(!store.paths().receipt_file(&setup.id).exists());
    assert!(store.paths().tombstone_file(&setup.id).exists());
    assert!(!store.paths().journal_file(&setup.op).exists());
    drop(store);
    let reopened = MarkdownStore::open(fixture.roots()).unwrap();
    assert!(matches!(
        reopened.read(&setup.id),
        Err(StoreError::Forgotten(_))
    ));
    assert!(effective(&reopened).is_empty());
}

#[test]
fn forget_recovery_regenerates_a_missing_tombstone() {
    let fixture = Fixture::new(MemoryScope::Project);
    let store = fixture.store();
    let setup = prepare_forget(&fixture, &store);
    fs::remove_file(fixture.current_file(&setup.id)).unwrap();
    fs::remove_file(&setup.tombstone_plan.temp_path).unwrap();
    let report = store.recover().unwrap();
    assert!(report.recovered.contains(&setup.op));
    assert!(store.paths().tombstone_file(&setup.id).exists());
    assert!(matches!(
        store.read(&setup.id),
        Err(StoreError::Forgotten(_))
    ));
    let tombstone = fs::read_to_string(store.paths().tombstone_file(&setup.id)).unwrap();
    assert!(!tombstone.contains("ACME-123"));
}

#[test]
fn a_changed_forget_target_blocks_instead_of_claiming_forgotten() {
    let fixture = Fixture::new(MemoryScope::Project);
    let store = fixture.store();
    let setup = prepare_forget(&fixture, &store);
    let changed = format!(
        "{}\nA third party changed this record.\n",
        fs::read_to_string(fixture.current_file(&setup.id)).unwrap()
    );
    fs::write(fixture.current_file(&setup.id), &changed).unwrap();
    let report = store.recover().unwrap();
    assert!(report.blocked.contains(&setup.op), "{report:?}");
    assert!(matches!(
        store.read(&setup.id),
        Err(StoreError::RecoveryConflict { .. })
    ));
    assert!(!store.paths().tombstone_file(&setup.id).exists());
    assert!(fixture.current_file(&setup.id).exists());
}

#[test]
fn orphaned_stage_files_are_cleaned_without_touching_referenced_files() {
    let fixture = Fixture::new(MemoryScope::Project);
    let store = fixture.store();
    let orphan = store.paths().stage_dir.join("orphan.md.stage");
    path::write_new(&orphan, b"orphan", 0o600).unwrap();
    let report = store.recover().unwrap();
    assert!(report.cleaned.contains(&orphan));
    assert!(!orphan.exists());
}

#[test]
fn stray_atomic_write_temporaries_are_cleaned() {
    let fixture = Fixture::new(MemoryScope::Project);
    let store = fixture.store();
    let temp = fixture.state.join(".tmp-4242-1-stray");
    fs::write(&temp, b"partial").unwrap();
    let report = store.recover().unwrap();
    assert_eq!(report.cleaned.len(), 1, "{:?}", report.cleaned);
    assert!(!temp.exists());
}
