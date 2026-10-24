//! E3 lifecycle contracts reinterpreted at the Stage 1 store level.
//!
//! The legacy `e3_lifecycle.rs` exercised the service chain: candidate parsing,
//! direct-command ownership, and the JSON store. Stage 1 has no extractor and no
//! service wiring, so each contract below is stated against the store's own
//! observable state. Where the legacy contract depended on semantic text
//! identity (U2 same body under different subjects), v2 deliberately diverges
//! and the divergence is asserted instead of hidden.

mod memory_v2_common;

use leveler_memory::v2::{
    Admission, Caller, ListQuery, MarkdownStore, MemoryAuthority, MemoryScope, MemoryStatus,
    StatementKind, StoreError,
};
use memory_v2_common::*;

/// U1 (store level): two writers of one keyed user fact never leave two active
/// records. The automatic writer is held as a pending proposal; whether the
/// turn itself is owned by a direct command is a Stage 2/3 service decision.
#[test]
fn u1_two_writers_of_one_keyed_fact_leave_one_active() {
    let fixture = Fixture::new(MemoryScope::Project);
    let store = fixture.store();
    let direct = create_user(
        &store,
        MemoryScope::Project,
        "billing_model",
        "subscription",
        "The provider bills by subscription.",
    );
    let direct_id = applied_id(&direct);

    let extractor = admit(
        &store,
        model(),
        fact_intent(
            MemoryScope::Project,
            Some("billing_model"),
            "usage",
            "The provider uses metered billing.",
        ),
    );
    assert!(
        matches!(extractor, Admission::Pending { .. }),
        "{extractor:?}"
    );
    let active = effective(&store);
    assert_eq!(active.len(), 1);
    assert_eq!(active[0].record.frontmatter.id, direct_id);
    assert_eq!(store.list_pending().unwrap().len(), 1);
}

/// U2 (store level): identical body text under a different subject is not proof
/// of identity. Stage 1 keeps both facts and reports the suspected duplicate;
/// it never applies Jaccard, synonyms, or any text threshold.
#[test]
fn u2_same_body_under_different_subject_stays_separate_with_a_duplicate_report() {
    let fixture = Fixture::new(MemoryScope::Project);
    let store = fixture.store();
    let first = create_user(
        &store,
        MemoryScope::Project,
        "billing_invoice_header",
        "ACME Ltd",
        "The invoice header uses ACME Ltd.",
    );
    let first_id = applied_id(&first);
    let second = create_user(
        &store,
        MemoryScope::Project,
        "billing_invoice_title",
        "ACME Ltd",
        "The invoice header uses ACME Ltd.",
    );
    let second_id = applied_id(&second);
    match second {
        Admission::Applied {
            possible_duplicate, ..
        } => assert!(
            possible_duplicate.contains(&first_id),
            "the store must report the suspected duplicate: {possible_duplicate:?}"
        ),
        other => panic!("expected a new active record, got {other:?}"),
    }
    assert_eq!(effective(&store).len(), 2);
    assert!(store.read(&first_id).is_ok());
    assert!(store.read(&second_id).is_ok());
}

/// U2 (store level): the same subject key converges on one current value.
#[test]
fn u2_a_reused_subject_keeps_one_current_value() {
    let fixture = Fixture::new(MemoryScope::Project);
    let store = fixture.store();
    let first = create_user(
        &store,
        MemoryScope::Project,
        "billing_invoice_header",
        "ACME Ltd",
        "The invoice header uses ACME Ltd.",
    );
    let first_id = applied_id(&first);
    create_user(
        &store,
        MemoryScope::Project,
        "billing_invoice_header",
        "ACME Ltd",
        "The invoice header still uses ACME Ltd.",
    );
    assert_eq!(effective(&store).len(), 1);
    assert!(matches!(
        store.read(&first_id),
        Err(StoreError::NotCurrent {
            status: MemoryStatus::Superseded,
            ..
        })
    ));
}

/// U3 (store level): a correction that names a keyless record's exact id
/// retires it. Keyless user facts are only creatable through migration, so the
/// fixture uses the migration caller and then an explicit user supersede.
#[test]
fn u3_a_correction_naming_a_keyless_memory_removes_it_from_current_truth() {
    let fixture = Fixture::new(MemoryScope::Project);
    let store = fixture.store();
    let legacy = admit(
        &store,
        Caller::Migration {
            run: "legacy-import".into(),
        },
        fact_intent(
            MemoryScope::Project,
            None,
            "subscription",
            "The provider bills by subscription.",
        ),
    );
    let old_id = applied_id(&legacy);
    assert!(
        store
            .read(&old_id)
            .unwrap()
            .record
            .frontmatter
            .subject_key
            .is_none()
    );

    let replacement = fact_intent(
        MemoryScope::Project,
        Some("billing_model"),
        "usage",
        "The provider bills by usage.",
    );
    let new_id = applied_id(
        &store
            .supersede(
                op(),
                user(),
                new_record_for(&replacement),
                std::slice::from_ref(&old_id),
            )
            .unwrap(),
    );
    let active = effective(&store);
    assert_eq!(
        active.len(),
        1,
        "the old keyless truth must not stay current"
    );
    assert_eq!(active[0].record.frontmatter.id, new_id);
    assert!(matches!(
        store.read(&old_id),
        Err(StoreError::NotCurrent {
            status: MemoryStatus::Superseded,
            ..
        })
    ));
}

/// U5 (store level): a fresh store handle sees exactly the current value. There
/// is no search layer in Stage 1, so the assertion covers read/effective/list.
#[test]
fn u5_a_new_session_sees_only_the_current_billing_model() {
    let fixture = Fixture::new(MemoryScope::Project);
    let store = fixture.store();
    create_user(
        &store,
        MemoryScope::Project,
        "billing_model",
        "subscription",
        "The provider bills by subscription.",
    );
    create_user(
        &store,
        MemoryScope::Project,
        "billing_model",
        "usage",
        "The provider bills by usage.",
    );
    drop(store);

    let reopened = MarkdownStore::open(fixture.roots()).unwrap();
    let active = effective(&reopened);
    assert_eq!(active.len(), 1, "a new session must see one truth");
    assert_eq!(active[0].record.frontmatter.value.as_deref(), Some("usage"));
    let listed = reopened.list(ListQuery::default()).unwrap();
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].value.as_deref(), Some("usage"));
}

/// U6a (store level): a model-authored proposal or correction cannot replace a
/// confirmed user decision. The statement label is structured metadata, not a
/// permission, and the write becomes a pending review item.
#[test]
fn u6a_model_statements_cannot_replace_a_formal_decision() {
    let fixture = Fixture::new(MemoryScope::Project);
    let store = fixture.store();
    create_user(
        &store,
        MemoryScope::Project,
        "billing_model",
        "subscription",
        "The provider bills by subscription.",
    );
    for statement in [
        StatementKind::Assertion,
        StatementKind::Correction,
        StatementKind::Note,
    ] {
        let mut intent = fact_intent(
            MemoryScope::Project,
            Some("billing_model"),
            "usage",
            "The provider bills by usage.",
        );
        intent.statement = statement;
        intent.proposed_authority = MemoryAuthority::UserExplicit;
        let admission = admit(&store, model(), intent);
        assert!(
            matches!(admission, Admission::Pending { .. }),
            "{statement:?} must stay pending: {admission:?}"
        );
    }
    let active = effective(&store);
    assert_eq!(active.len(), 1);
    assert_eq!(
        active[0].record.frontmatter.value.as_deref(),
        Some("subscription")
    );
    assert_eq!(store.list_pending().unwrap().len(), 3);
}

/// U6a counter-case (store level): an explicit user correction replaces the
/// formal decision and keeps the old value in history.
#[test]
fn u6a_a_confirmed_correction_does_replace_the_formal_decision() {
    let fixture = Fixture::new(MemoryScope::Project);
    let store = fixture.store();
    let first = create_user(
        &store,
        MemoryScope::Project,
        "billing_model",
        "subscription",
        "The provider bills by subscription.",
    );
    let old_id = applied_id(&first);
    create_user(
        &store,
        MemoryScope::Project,
        "billing_model",
        "usage",
        "The provider bills by usage.",
    );
    let active = effective(&store);
    assert_eq!(active.len(), 1);
    assert_eq!(active[0].record.frontmatter.value.as_deref(), Some("usage"));
    assert_eq!(
        active[0].record.frontmatter.authority,
        MemoryAuthority::UserExplicit
    );
    assert_eq!(
        store.history(&old_id, 1).unwrap().frontmatter.status,
        MemoryStatus::Active
    );
}

/// Covered pending cleanup is exact and traceable. A model proposal for a
/// Global fact is pending; an explicit user command that states the same key,
/// value, and body clears it and records why.
#[test]
fn a_covered_pending_is_cleared_with_a_traceable_disposition() {
    let fixture = Fixture::new(MemoryScope::Global);
    let store = fixture.store();
    let pending = admit(
        &store,
        model(),
        fact_intent(
            MemoryScope::Global,
            Some("billing_model"),
            "subscription",
            "The provider bills by subscription.",
        ),
    );
    let pending = pending_id(&pending);
    assert_eq!(store.list_pending().unwrap().len(), 1);

    create_user(
        &store,
        MemoryScope::Global,
        "billing_model",
        "subscription",
        "The provider bills by subscription.",
    );
    assert!(store.list_pending().unwrap().is_empty());
    let history = std::fs::read_to_string(fixture.state.join("pending_history.jsonl"))
        .expect("every pending settlement must leave a disposition record");
    assert!(history.contains(&pending.to_string()));
    assert!(history.contains("covered"));
}

/// A pending proposal with a different value is an undecided alternative and
/// must survive an unrelated commit of the same subject.
#[test]
fn a_pending_with_a_different_value_survives_a_commit_of_the_same_subject() {
    let fixture = Fixture::new(MemoryScope::Global);
    let store = fixture.store();
    admit(
        &store,
        model(),
        fact_intent(
            MemoryScope::Global,
            Some("billing_model"),
            "usage",
            "The provider might switch to usage billing.",
        ),
    );
    create_user(
        &store,
        MemoryScope::Global,
        "billing_model",
        "subscription",
        "The provider bills by subscription.",
    );
    let pending = store.list_pending().unwrap();
    assert_eq!(pending.len(), 1);
    assert_eq!(pending[0].frontmatter.value.as_deref(), Some("usage"));
}

/// Guard against the identity rules widening into "drop every proposal": a
/// plain new confirmed fact still becomes current.
#[test]
fn a_new_confirmed_fact_still_becomes_current() {
    let fixture = Fixture::new(MemoryScope::Project);
    let store = fixture.store();
    create_user(
        &store,
        MemoryScope::Project,
        "release_probe_codename",
        "ORANGE-7319",
        "The release probe codename is ORANGE-7319.",
    );
    assert_eq!(effective(&store).len(), 1);
}
