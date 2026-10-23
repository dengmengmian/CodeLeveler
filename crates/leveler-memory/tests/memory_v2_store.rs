//! Stage 1 store contracts: identity, CAS, lifecycle state and history.

mod memory_v2_common;

use std::fs;

use leveler_memory::v2::{
    Admission, AdmitRequest, ForgetOutcome, ListQuery, MarkdownStore, MemoryAuthority, MemoryKind,
    MemoryScope, MemoryStatus, RecordPatch, StoreError, TrustSummary,
};
use memory_v2_common::*;

#[test]
fn create_read_and_cas_update_keep_history() {
    let fixture = Fixture::new(MemoryScope::Project);
    let store = fixture.store();
    let created = create_user(
        &store,
        MemoryScope::Project,
        "billing_model",
        "monthly",
        "The provider bills monthly.",
    );
    let id = applied_id(&created);
    let read = store.read(&id).unwrap();
    assert_eq!(
        read.record.frontmatter.authority,
        MemoryAuthority::UserExplicit
    );
    assert_eq!(read.trust, TrustSummary::Trusted);
    assert_eq!(read.cas.revision, 1);
    assert!(fixture.current_file(&id).exists());
    assert!(
        fixture
            .state
            .join("receipts")
            .join(format!("{id}.json"))
            .exists()
    );
    assert!(
        fixture
            .state
            .join("history")
            .join(id.as_str())
            .join("0001.md")
            .exists()
    );

    let patch = RecordPatch {
        body: Some("The provider bills monthly, invoiced on the first.".into()),
        ..Default::default()
    };
    let caller = user();
    let updated = store
        .update(op(), caller, &id, patch.clone(), read.cas.clone())
        .unwrap();
    assert!(updated.is_applied());
    let read2 = store.read(&id).unwrap();
    assert_eq!(read2.cas.revision, 2);
    assert!(read2.record.body.contains("invoiced"));
    let old = store.history(&id, 1).unwrap();
    assert_eq!(old.body, "The provider bills monthly.");

    let stale = store.update(op(), user(), &id, patch, read.cas).unwrap();
    assert!(matches!(stale, Admission::Conflict { .. }), "{stale:?}");
}

#[test]
fn retrying_the_same_operation_is_idempotent_and_reuse_is_rejected() {
    let fixture = Fixture::new(MemoryScope::Project);
    let store = fixture.store();
    let op_id = op();
    let caller = user();
    let intent = fact_intent(
        MemoryScope::Project,
        Some("billing_model"),
        "monthly",
        "The provider bills monthly.",
    );
    let first = store
        .admit(AdmitRequest {
            op_id: op_id.clone(),
            caller: caller.clone(),
            intent: intent.clone(),
            expected: None,
        })
        .unwrap();
    assert!(matches!(first, Admission::Applied { .. }));
    let second = store
        .admit(AdmitRequest {
            op_id: op_id.clone(),
            caller: caller.clone(),
            intent: intent.clone(),
            expected: None,
        })
        .unwrap();
    assert!(matches!(second, Admission::Replayed { .. }));
    assert_eq!(effective(&store).len(), 1);

    let different = fact_intent(
        MemoryScope::Project,
        Some("billing_model"),
        "usage",
        "The provider bills by usage.",
    );
    let error = store
        .admit(AdmitRequest {
            op_id: op_id,
            caller,
            intent: different,
            expected: None,
        })
        .unwrap_err();
    assert!(matches!(error, StoreError::OpIdReuse(_)), "{error:?}");
}

#[test]
fn explicit_supersede_retires_the_target_and_keeps_it_in_history() {
    let fixture = Fixture::new(MemoryScope::Project);
    let store = fixture.store();
    let old = create_user(
        &store,
        MemoryScope::Project,
        "billing_model",
        "subscription",
        "The provider bills by subscription.",
    );
    let old_id = applied_id(&old);
    let replacement = fact_intent(
        MemoryScope::Project,
        Some("billing_model"),
        "usage",
        "The provider bills by usage.",
    );
    let replacement_id = applied_id(
        &store
            .supersede(
                op(),
                user(),
                new_record_for(&replacement),
                &[old_id.clone()],
            )
            .unwrap(),
    );
    assert_ne!(old_id, replacement_id);
    assert!(matches!(
        store.read(&old_id),
        Err(StoreError::NotCurrent {
            status: MemoryStatus::Superseded,
            ..
        })
    ));
    assert_eq!(effective(&store).len(), 1);
    assert_eq!(effective(&store)[0].record.frontmatter.id, replacement_id);
    let history = store.history(&old_id, 1).unwrap();
    assert_eq!(history.frontmatter.status, MemoryStatus::Active);
    assert_eq!(history.body, "The provider bills by subscription.");
}

#[test]
fn superseded_attic_points_at_the_replacement_that_became_current() {
    let fixture = Fixture::new(MemoryScope::Project);
    let store = fixture.store();
    let old_id = applied_id(&create_user(
        &store,
        MemoryScope::Project,
        "billing_model",
        "subscription",
        "The provider bills by subscription.",
    ));
    let replacement = fact_intent(
        MemoryScope::Project,
        Some("billing_model"),
        "usage",
        "The provider bills by usage.",
    );
    let replacement_id = applied_id(
        &store
            .supersede(
                op(),
                user(),
                new_record_for(&replacement),
                &[old_id.clone()],
            )
            .unwrap(),
    );
    let attic =
        fs::read_to_string(fixture.state.join("attic").join(format!("{old_id}.json"))).unwrap();
    let attic: serde_json::Value = serde_json::from_str(&attic).unwrap();
    assert_eq!(
        attic["superseded_by"].as_str(),
        Some(replacement_id.as_str()),
        "the retired record must point at the id that is actually current"
    );
}

#[test]
fn deprecate_removes_current_truth_but_keeps_history() {
    let fixture = Fixture::new(MemoryScope::Project);
    let store = fixture.store();
    let created = create_user(
        &store,
        MemoryScope::Project,
        "billing_model",
        "monthly",
        "The provider bills monthly.",
    );
    let id = applied_id(&created);
    let cas = store.read(&id).unwrap().cas;
    let result = store
        .deprecate(
            op(),
            user(),
            &id,
            cas,
            leveler_memory::v2::Reason::new("outdated"),
        )
        .unwrap();
    assert!(result.is_applied());
    assert!(matches!(
        store.read(&id),
        Err(StoreError::NotCurrent {
            status: MemoryStatus::Deprecated,
            ..
        })
    ));
    assert!(effective(&store).is_empty());
    assert_eq!(
        store.history(&id, 1).unwrap().body,
        "The provider bills monthly."
    );
}

#[test]
fn pinning_is_a_revision_and_a_model_cannot_undo_a_user_pin() {
    let fixture = Fixture::new(MemoryScope::Project);
    let store = fixture.store();
    let created = create_user(
        &store,
        MemoryScope::Project,
        "coding_style",
        "direct",
        "Prefer direct answers.",
    );
    let id = applied_id(&created);
    let cas = store.read(&id).unwrap().cas;
    let pinned = store
        .pin(op(), user(), &id, leveler_memory::v2::PinOwner::User, cas)
        .unwrap();
    assert!(pinned.is_applied());
    let read = store.read(&id).unwrap();
    assert!(read.record.frontmatter.pinned);
    assert_eq!(
        read.record.frontmatter.pinned_by,
        Some(leveler_memory::v2::PinOwner::User)
    );
    assert_eq!(read.cas.revision, 2);

    let model_unpin = store.unpin(op(), model(), &id, read.cas.clone()).unwrap();
    assert!(
        matches!(model_unpin, Admission::Pending { .. }),
        "{model_unpin:?}"
    );
    assert!(store.read(&id).unwrap().record.frontmatter.pinned);

    let user_unpin = store.unpin(op(), user(), &id, read.cas).unwrap();
    assert!(user_unpin.is_applied());
    assert!(!store.read(&id).unwrap().record.frontmatter.pinned);
}

#[test]
fn ttl_expiration_is_mechanical_and_survives_reopen() {
    let fixture = Fixture::new(MemoryScope::Project);
    let store = fixture.store();
    let mut intent = fact_intent(
        MemoryScope::Project,
        Some("temporary_flag"),
        "on",
        "A temporary flag.",
    );
    intent.expires_at = Some(chrono::Utc::now() + chrono::Duration::seconds(60));
    let id = applied_id(&admit(&store, user(), intent));
    assert_eq!(effective(&store).len(), 1);
    let expired = store
        .expire_due(chrono::Utc::now() + chrono::Duration::seconds(120))
        .unwrap();
    assert_eq!(expired, vec![id.clone()]);
    assert!(matches!(
        store.read(&id),
        Err(StoreError::NotCurrent {
            status: MemoryStatus::Expired,
            ..
        })
    ));
    assert!(effective(&store).is_empty());
    assert_eq!(store.history(&id, 1).unwrap().body, "A temporary flag.");
    drop(store);

    let reopened = MarkdownStore::open(fixture.roots()).unwrap();
    assert!(effective(&reopened).is_empty());
}

#[test]
fn forget_removes_body_copies_and_leaves_only_a_minimal_tombstone() {
    let fixture = Fixture::new(MemoryScope::Project);
    let store = fixture.store();
    let created = create_user(
        &store,
        MemoryScope::Project,
        "account_handle",
        "ACME-123",
        "The account handle is ACME-123.",
    );
    let id = applied_id(&created);
    let cas = store.read(&id).unwrap().cas;
    let caller = user();
    let outcome = store
        .forget(op(), caller.clone(), &id, cas.clone())
        .unwrap();
    assert!(matches!(outcome, ForgetOutcome::Forgotten(_)));
    assert!(matches!(store.read(&id), Err(StoreError::Forgotten(_))));
    assert!(matches!(
        store.history(&id, 1),
        Err(StoreError::Forgotten(_))
    ));
    assert!(store.list(ListQuery::default()).unwrap().is_empty());
    assert!(effective(&store).is_empty());
    assert!(!fixture.current_file(&id).exists());
    assert!(!fixture.state.join("history").join(id.as_str()).exists());
    assert!(
        !fixture
            .state
            .join("receipts")
            .join(format!("{id}.json"))
            .exists()
    );
    let tombstone_text =
        fs::read_to_string(fixture.state.join("tombstones").join(format!("{id}.json"))).unwrap();
    assert!(!tombstone_text.contains("ACME-123"));
    assert!(!tombstone_text.contains("account handle is"));
    assert!(tombstone_text.contains("subject_key_sha256"));
    assert!(tombstone_text.contains("content_sha256"));

    let fresh = store.forget(op(), user(), &id, cas);
    assert!(matches!(fresh, Err(StoreError::Forgotten(_))), "{fresh:?}");
}

#[test]
fn forget_is_idempotent_for_the_same_operation_identity() {
    let fixture = Fixture::new(MemoryScope::Project);
    let store = fixture.store();
    let created = create_user(
        &store,
        MemoryScope::Project,
        "account_handle",
        "ACME-123",
        "The account handle is ACME-123.",
    );
    let id = applied_id(&created);
    let cas = store.read(&id).unwrap().cas;
    let caller = user();
    let op_id = op();
    let first = store
        .forget(op_id.clone(), caller.clone(), &id, cas.clone())
        .unwrap();
    assert!(matches!(first, ForgetOutcome::Forgotten(_)));
    let second = store.forget(op_id, caller, &id, cas).unwrap();
    assert!(matches!(second, ForgetOutcome::Forgotten(_)));

    drop(store);
    let reopened = MarkdownStore::open(fixture.roots()).unwrap();
    assert!(matches!(reopened.read(&id), Err(StoreError::Forgotten(_))));
}

#[cfg(unix)]
#[test]
fn forget_reports_a_remnant_it_cannot_remove() {
    use std::os::unix::fs::symlink;

    let fixture = Fixture::new(MemoryScope::Project);
    let store = fixture.store();
    let created = create_user(
        &store,
        MemoryScope::Project,
        "account_handle",
        "ACME-123",
        "The account handle is ACME-123.",
    );
    let id = applied_id(&created);
    let cas = store.read(&id).unwrap().cas;
    let attic = fixture.state.join("attic").join(format!("{id}.json"));
    let moved = fixture.dir().join("moved-attic");
    fs::write(&moved, b"{}").unwrap();
    symlink(&moved, &attic).unwrap();

    let outcome = store.forget(op(), user(), &id, cas);
    assert!(
        outcome.is_err(),
        "forget must not claim erasure it could not perform: {outcome:?}"
    );

    fs::remove_file(&attic).unwrap();
}

#[test]
fn list_filters_without_exposing_bodies() {
    let fixture = Fixture::new(MemoryScope::Project);
    let store = fixture.store();
    create_user(
        &store,
        MemoryScope::Project,
        "billing_model",
        "monthly",
        "The provider bills monthly.",
    );
    create_user(
        &store,
        MemoryScope::Project,
        "release_codename",
        "ORANGE-7319",
        "The release codename is ORANGE-7319.",
    );
    let all = store.list(ListQuery::default()).unwrap();
    assert_eq!(all.len(), 2);
    let limited = store
        .list(ListQuery {
            limit: Some(1),
            ..Default::default()
        })
        .unwrap();
    assert_eq!(limited.len(), 1);
    let experiences = store
        .list(ListQuery {
            kind: Some(MemoryKind::Experience),
            ..Default::default()
        })
        .unwrap();
    assert!(experiences.is_empty());
}

#[test]
fn non_memory_files_in_store_directories_are_ignored() {
    let fixture = Fixture::new(MemoryScope::Project);
    let store = fixture.store();
    create_user(
        &store,
        MemoryScope::Project,
        "billing_model",
        "monthly",
        "The provider bills monthly.",
    );
    fs::write(fixture.current.join("notes.txt"), b"not a record").unwrap();
    fs::write(
        fixture.state.join("journal").join("stray.txt"),
        b"not a journal",
    )
    .unwrap();

    assert_eq!(store.list(ListQuery::default()).unwrap().len(), 1);
    drop(store);
    let reopened = MarkdownStore::open(fixture.roots()).unwrap();
    assert_eq!(effective(&reopened).len(), 1);
}

#[test]
fn runtime_experiences_need_runtime_evidence_before_becoming_active() {
    let fixture = Fixture::new(MemoryScope::Project);
    let store = fixture.store();
    let model_experience = admit(
        &store,
        model(),
        experience_intent(
            MemoryScope::Project,
            "build",
            "cargo build fails",
            "Use a clean target directory.",
            true,
        ),
    );
    assert!(matches!(
        model_experience,
        Admission::Pending {
            reason: leveler_memory::v2::PendingReason::ExperienceRequiresRuntimeEvidence,
            ..
        }
    ));
    assert!(store.list_pending().unwrap().len() == 1);
    assert!(effective(&store).is_empty());

    let runtime_experience = admit(
        &store,
        runtime(),
        experience_intent(
            MemoryScope::Project,
            "build",
            "cargo build fails",
            "Use a clean target directory.",
            true,
        ),
    );
    assert!(runtime_experience.is_applied());
    let active = effective(&store);
    assert_eq!(active.len(), 1);
    assert_eq!(
        active[0].record.frontmatter.authority,
        MemoryAuthority::RuntimeObserved
    );
}

/// Orphan cleanup is a lifecycle event, not a per-operation cost.
///
/// Opening a store recovers journals and scans the private state tree once.
/// Reads and writes of an already-open store must not repeat that scan: at
/// scale the history tree holds one directory per record, so a scan per read
/// would dominate every recall. The leftovers are still cleaned — at the next
/// open, and on an explicit recovery.
#[test]
fn orphan_cleanup_is_a_lifecycle_event_not_a_read_cost() {
    let fixture = Fixture::new(MemoryScope::Project);
    let store = fixture.store();
    let created = create_user(
        &store,
        MemoryScope::Project,
        "billing_model",
        "monthly",
        "The provider bills monthly.",
    );
    let id = applied_id(&created);
    let read = store.read(&id).unwrap();

    let orphan = store.paths().stage_dir.join("orphan.md.stage");
    leveler_memory::v2::path::write_new(&orphan, b"orphan", 0o600).unwrap();
    let stray = fixture.state.join(".tmp-4242-1-stray");
    fs::write(&stray, b"partial").unwrap();

    // Read, list and write through the open handle.
    store.read(&id).unwrap();
    store.effective_active().unwrap();
    let updated = store
        .update(
            leveler_memory::v2::OpId::parse("op-lifecycle-cleanup").unwrap(),
            leveler_memory::v2::Caller::UserCommand {
                invocation: "invocation-lifecycle-cleanup".into(),
            },
            &id,
            RecordPatch {
                value: Some("yearly".into()),
                body: Some("The provider bills yearly.".into()),
                ..Default::default()
            },
            read.cas,
        )
        .unwrap();
    assert!(updated.is_applied(), "{updated:?}");

    assert!(
        orphan.exists(),
        "a read or a write must not scan the state tree for orphans"
    );
    assert!(
        stray.exists(),
        "a read or a write must not scan for temporaries"
    );

    // Reopening is the boundary that pays for the scan.
    let reopened = fixture.store();
    assert!(
        !orphan.exists(),
        "reopening the store must clean the orphan"
    );
    assert!(
        !stray.exists(),
        "reopening the store must clean the temporary"
    );

    // An explicit recovery is the other boundary.
    let orphan_again = reopened.paths().stage_dir.join("orphan.md.stage");
    leveler_memory::v2::path::write_new(&orphan_again, b"orphan", 0o600).unwrap();
    let report = reopened.recover().unwrap();
    assert!(report.cleaned.contains(&orphan_again), "{report:?}");
    assert!(!orphan_again.exists());
}
