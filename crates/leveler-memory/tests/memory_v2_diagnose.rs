//! Read-only diagnostics for a Markdown scope.
//!
//! A broken scope must be reportable and must never look like an empty one.
//! These tests also pin that `diagnose` itself changes nothing: no directory
//! creation, no recovery, no repair.

mod memory_v2_common;

use leveler_memory::v2::{
    DiagnosticKind, DiagnosticSeverity, ForgetOutcome, JournalPlan, ListQuery, MarkdownStore,
    MemoryKind, MemoryScope, MemoryStatus, Operation, Receipt, StoreError, frontmatter,
    sha256_hex,
};
use memory_v2_common::*;

fn diagnose(fixture: &Fixture) -> leveler_memory::v2::DiagnosticReport {
    MarkdownStore::diagnose(fixture.roots()).unwrap()
}

fn issues_of(
    report: &leveler_memory::v2::DiagnosticReport,
    kind: DiagnosticKind,
) -> Vec<&leveler_memory::v2::DiagnosticIssue> {
    report
        .issues
        .iter()
        .filter(|issue| issue.kind == kind)
        .collect()
}

#[test]
fn an_uninitialized_scope_is_reported_without_creating_it() {
    let fixture = Fixture::new(MemoryScope::Project);
    let report = diagnose(&fixture);
    assert!(!report.initialized);
    assert_eq!(report.active_records, 0);
    assert!(report.issues.is_empty(), "{:?}", report.issues);
    assert!(
        !fixture.current.exists() && !fixture.state.exists(),
        "diagnose created the store it was supposed to inspect"
    );
}

#[test]
fn a_clean_scope_reports_no_issues() {
    let fixture = Fixture::new(MemoryScope::Project);
    let store = fixture.store();
    let _ = create_user(
        &store,
        MemoryScope::Project,
        "billing_model",
        "subscription",
        "The provider bills by subscription.",
    );
    drop(store);

    let report = diagnose(&fixture);
    assert!(report.initialized);
    assert_eq!(report.active_records, 1);
    assert_eq!(report.pending_records, 0);
    assert!(report.issues.is_empty(), "{:?}", report.issues);
    assert!(!report.needs_action());
}

#[test]
fn a_blocked_journal_is_blocking_and_reads_do_not_look_empty() {
    let fixture = Fixture::new(MemoryScope::Project);
    let store = fixture.store();
    let _ = create_user(
        &store,
        MemoryScope::Project,
        "billing_model",
        "subscription",
        "The provider bills by subscription.",
    );
    let paths = store.paths().clone();
    drop(store);

    let op = op();
    let mut plan = JournalPlan::new(op.clone(), Operation::Create, "runtime");
    plan.write(&paths.journal_file(&op)).unwrap();
    plan.mark_blocked(&paths.journal_file(&op), "target changed to a third state")
        .unwrap();

    let report = diagnose(&fixture);
    let blocked = issues_of(&report, DiagnosticKind::BlockedJournal);
    assert_eq!(blocked.len(), 1, "{:?}", report.issues);
    assert_eq!(blocked[0].severity, DiagnosticSeverity::Blocking);
    assert!(blocked[0].detail.contains("third state"));
    assert!(report.is_blocking());

    let store = fixture.store();
    let error = store.effective_active().unwrap_err();
    assert!(
        matches!(error, StoreError::RecoveryConflict { .. }),
        "a blocked scope reported: {error:?}"
    );
    assert!(matches!(
        store.list(ListQuery::default()),
        Err(StoreError::RecoveryConflict { .. })
    ));

    // Reading did not resolve or delete the journal.
    assert!(paths.journal_file(&op).exists());
}

#[test]
fn an_invalid_current_document_is_blocking_and_never_skipped() {
    let fixture = Fixture::new(MemoryScope::Project);
    let store = fixture.store();
    let _ = create_user(
        &store,
        MemoryScope::Project,
        "billing_model",
        "subscription",
        "The provider bills by subscription.",
    );
    drop(store);
    std::fs::write(fixture.current.join("broken.md"), "not a memory document").unwrap();

    let report = diagnose(&fixture);
    let invalid = issues_of(&report, DiagnosticKind::InvalidCurrentDocument);
    assert_eq!(invalid.len(), 1, "{:?}", report.issues);
    assert!(report.is_blocking());

    let store = fixture.store();
    assert!(matches!(
        store.effective_active(),
        Err(StoreError::ExternalConflict { .. })
    ));
}

#[test]
fn an_external_edit_is_action_required_and_withheld_from_truth() {
    let fixture = Fixture::new(MemoryScope::Project);
    let store = fixture.store();
    let admission = create_user(
        &store,
        MemoryScope::Project,
        "billing_model",
        "subscription",
        "The provider bills by subscription.",
    );
    let id = applied_id(&admission);
    drop(store);

    let path = fixture.current_file(&id);
    let mut text = std::fs::read_to_string(&path).unwrap();
    text.push_str("\nAn edit made outside the store.\n");
    std::fs::write(&path, text).unwrap();

    let report = diagnose(&fixture);
    let unverified = issues_of(&report, DiagnosticKind::ExternalUnverified);
    assert_eq!(unverified.len(), 1, "{:?}", report.issues);
    assert_eq!(unverified[0].severity, DiagnosticSeverity::ActionRequired);
    assert_ne!(report.active_records, 1);

    let store = fixture.store();
    assert!(
        store.effective_active().unwrap().is_empty(),
        "an unverified external edit became truth"
    );
    let listed = store
        .list(ListQuery {
            scope: None,
            kind: None,
            status: Some(MemoryStatus::Active),
            limit: None,
            include_external: true,
        })
        .unwrap();
    assert_eq!(listed.len(), 1);
    assert_eq!(
        listed[0].trust,
        leveler_memory::v2::TrustSummary::ExternalUnverified
    );
}

#[test]
fn two_trusted_records_for_one_subject_are_reported_as_an_invariant_violation() {
    let fixture = Fixture::new(MemoryScope::Project);
    let store = fixture.store();
    let admission = create_user(
        &store,
        MemoryScope::Project,
        "billing_model",
        "subscription",
        "The provider bills by subscription.",
    );
    let id = applied_id(&admission);
    let current = std::fs::read(fixture.current_file(&id)).unwrap();
    let mut copy = frontmatter::parse(&current).unwrap();
    copy.frontmatter.id = leveler_memory::v2::MemoryId::new();
    let bytes = frontmatter::render(&copy).unwrap();
    let copy_id = copy.frontmatter.id.clone();
    std::fs::write(fixture.current_file(&copy_id), &bytes).unwrap();
    let receipt = Receipt::from_record(&copy, &sha256_hex(&bytes));
    std::fs::write(
        store.paths().receipt_file(&copy_id),
        receipt.to_json().unwrap(),
    )
    .unwrap();
    drop(store);

    let report = diagnose(&fixture);
    let duplicates = issues_of(&report, DiagnosticKind::DuplicateActiveSubject);
    assert_eq!(duplicates.len(), 1, "{:?}", report.issues);
    assert_eq!(duplicates[0].severity, DiagnosticSeverity::Blocking);
    assert_eq!(report.active_records, 2);
}

#[test]
fn pending_obligations_count_without_being_activated() {
    let fixture = Fixture::new(MemoryScope::Project);
    let store = fixture.store();
    let admission = create_user(
        &store,
        MemoryScope::Project,
        "billing_model",
        "subscription",
        "The provider bills by subscription.",
    );
    let id = applied_id(&admission);
    let cas = store.read(&id).unwrap().cas;
    let pending = store
        .update(
            op(),
            model(),
            &id,
            leveler_memory::v2::RecordPatch {
                value: Some("usage".into()),
                body: Some("The provider bills by usage.".into()),
                ..Default::default()
            },
            cas,
        )
        .unwrap();
    assert!(matches!(pending, leveler_memory::v2::Admission::Pending { .. }));
    drop(store);

    let report = diagnose(&fixture);
    assert_eq!(report.pending_records, 1);
    assert_eq!(report.active_records, 1);
    assert!(report.issues.is_empty(), "{:?}", report.issues);
}

#[test]
fn orphan_stage_files_are_warnings_not_truth() {
    let fixture = Fixture::new(MemoryScope::Project);
    let store = fixture.store();
    let stage = store.paths().stage_dir.clone();
    drop(store);
    std::fs::write(stage.join("abandoned.md.stage"), "half a record").unwrap();

    let report = diagnose(&fixture);
    let orphans = issues_of(&report, DiagnosticKind::OrphanStageFile);
    assert_eq!(orphans.len(), 1, "{:?}", report.issues);
    assert_eq!(orphans[0].severity, DiagnosticSeverity::Warning);
    assert!(!report.needs_action());
}

#[test]
fn a_prepared_journal_is_reported_and_left_untouched() {
    let fixture = Fixture::new(MemoryScope::Project);
    let store = fixture.store();
    let paths = store.paths().clone();
    drop(store);

    let op = op();
    let plan = JournalPlan::new(op.clone(), Operation::Create, "runtime");
    plan.write(&paths.journal_file(&op)).unwrap();

    let report = diagnose(&fixture);
    let pending = issues_of(&report, DiagnosticKind::PendingJournal);
    assert_eq!(pending.len(), 1, "{:?}", report.issues);
    assert_eq!(pending[0].severity, DiagnosticSeverity::Warning);
    assert!(
        paths.journal_file(&op).exists(),
        "diagnose recovered a transaction it must only report"
    );
}

#[test]
fn a_forgotten_record_does_not_look_like_an_empty_scope() {
    let fixture = Fixture::new(MemoryScope::Project);
    let store = fixture.store();
    let admission = create_user(
        &store,
        MemoryScope::Project,
        "billing_model",
        "subscription",
        "The provider bills by subscription.",
    );
    let id = applied_id(&admission);
    let cas = store.read(&id).unwrap().cas;
    let outcome = store
        .forget(op(), user(), &id, cas)
        .unwrap();
    assert!(matches!(outcome, ForgetOutcome::Forgotten(_)));
    drop(store);

    let report = diagnose(&fixture);
    assert_eq!(report.active_records, 0);
    assert!(report.issues.is_empty(), "{:?}", report.issues);
    let store = fixture.store();
    assert!(matches!(store.read(&id), Err(StoreError::Forgotten(_))));
    assert!(
        store
            .list(ListQuery {
                scope: None,
                kind: Some(MemoryKind::UserFact),
                status: None,
                limit: None,
                include_external: true,
            })
            .unwrap()
            .is_empty()
    );
}
