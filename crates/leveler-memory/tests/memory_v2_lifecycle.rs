//! Stage 1 lifecycle contracts: proposals, review, and authority derivation.

mod memory_v2_common;

use leveler_memory::v2::{
    Admission, Caller, MemoryAuthority, MemoryKind, MemoryScope, MemoryStatus, Operation,
    PendingReason, PinOwner, Reason, ReviewDecision, StoreError,
};
use memory_v2_common::*;

#[test]
fn a_model_proposal_cannot_replace_a_user_decision_without_review() {
    let fixture = Fixture::new(MemoryScope::Project);
    let store = fixture.store();
    let original = create_user(
        &store,
        MemoryScope::Project,
        "billing_model",
        "subscription",
        "The provider bills by subscription.",
    );
    let original_id = applied_id(&original);

    let mut forged = fact_intent(
        MemoryScope::Project,
        Some("billing_model"),
        "usage",
        "The provider bills by usage.",
    );
    forged.proposed_authority = MemoryAuthority::UserExplicit;
    let proposal = admit(&store, model(), forged);
    let pending = pending_id(&proposal);
    assert!(matches!(
        proposal,
        Admission::Pending {
            reason: PendingReason::ProtectedUserFact,
            ..
        }
    ));
    let unchanged = store.read(&original_id).unwrap();
    assert_eq!(
        unchanged.record.frontmatter.value.as_deref(),
        Some("subscription")
    );
    assert_eq!(store.list_pending().unwrap().len(), 1);
    assert_eq!(
        store.list_pending().unwrap()[0].frontmatter.authority,
        MemoryAuthority::ModelInferred
    );

    let accepted = store
        .review(op(), user_review(), &pending, ReviewDecision::Accept)
        .unwrap();
    assert!(accepted.is_applied(), "{accepted:?}");
    assert!(store.list_pending().unwrap().is_empty());
    let effective = effective(&store);
    assert_eq!(effective.len(), 1);
    assert_eq!(
        effective[0].record.frontmatter.value.as_deref(),
        Some("usage")
    );
    assert_eq!(
        effective[0].record.frontmatter.authority,
        MemoryAuthority::UserApproved
    );
    assert!(matches!(
        store.read(&original_id),
        Err(StoreError::NotCurrent {
            status: MemoryStatus::Superseded,
            ..
        })
    ));
}

/// A confirmed user statement is evidence, not a user decision. A later
/// statement under the same identity with a new span supersedes it directly;
/// only `user_explicit` / `user_approved` targets need a person's review.
#[test]
fn a_user_stated_fact_is_corrected_by_a_new_user_statement() {
    let fixture = Fixture::new(MemoryScope::Project);
    let store = fixture.store();
    let mut first = fact_intent(
        MemoryScope::Project,
        Some("release_window"),
        "tuesday",
        "The release runs in the Tuesday window.",
    );
    first.evidence = vec![source_ref()];
    let original_id = applied_id(&admit(&store, runtime(), first));

    let mut correction = fact_intent(
        MemoryScope::Project,
        Some("release_window"),
        "friday",
        "The release runs in the Friday window.",
    );
    let mut later = source_ref();
    later.turn_id = Some("turn-2".into());
    later.evidence_span = Some("move the release to Friday".into());
    correction.evidence = vec![later];
    let admission = admit(&store, runtime(), correction);
    assert!(
        matches!(
            admission,
            Admission::Applied {
                operation: Operation::Supersede,
                ..
            }
        ),
        "{admission:?}"
    );
    let corrected_id = applied_id(&admission);
    assert_ne!(corrected_id, original_id);

    let effective = effective(&store);
    assert_eq!(effective.len(), 1);
    assert_eq!(
        effective[0].record.frontmatter.value.as_deref(),
        Some("friday")
    );
    assert_eq!(
        effective[0].record.frontmatter.authority,
        MemoryAuthority::UserStated
    );
    assert!(
        effective[0]
            .record
            .frontmatter
            .supersedes
            .contains(&original_id.to_string())
    );
    assert!(matches!(
        store.read(&original_id),
        Err(StoreError::NotCurrent {
            status: MemoryStatus::Superseded,
            ..
        })
    ));
}

#[test]
fn a_model_still_cannot_replace_a_user_stated_fact() {
    let fixture = Fixture::new(MemoryScope::Project);
    let store = fixture.store();
    let mut first = fact_intent(
        MemoryScope::Project,
        Some("release_window"),
        "tuesday",
        "The release runs in the Tuesday window.",
    );
    first.evidence = vec![source_ref()];
    let original_id = applied_id(&admit(&store, runtime(), first));

    let proposal = admit(
        &store,
        model(),
        fact_intent(
            MemoryScope::Project,
            Some("release_window"),
            "friday",
            "The release runs in the Friday window.",
        ),
    );
    assert!(
        matches!(
            proposal,
            Admission::Pending {
                reason: PendingReason::LowerAuthority,
                ..
            }
        ),
        "{proposal:?}"
    );
    let current = store.read(&original_id).unwrap();
    assert_eq!(current.record.frontmatter.value.as_deref(), Some("tuesday"));
}

/// A model pin of any user-owned fact still waits for a person (§7.3), even
/// though a user statement is not protected from a same-authority correction.
#[test]
fn a_model_pin_of_a_user_stated_fact_waits_for_a_person() {
    let fixture = Fixture::new(MemoryScope::Project);
    let store = fixture.store();
    let mut intent = fact_intent(
        MemoryScope::Project,
        Some("coding_style"),
        "direct",
        "Prefer direct answers.",
    );
    intent.evidence = vec![source_ref()];
    let id = applied_id(&admit(&store, runtime(), intent));
    let cas = store.read(&id).unwrap().cas;

    let model_pin = store
        .pin(op(), model(), &id, PinOwner::Model, cas.clone())
        .unwrap();
    assert!(
        matches!(
            model_pin,
            Admission::Pending {
                reason: PendingReason::ProtectedUserFact,
                ..
            }
        ),
        "{model_pin:?}"
    );
    assert!(!store.read(&id).unwrap().record.frontmatter.pinned);

    let user_pin = store.pin(op(), user(), &id, PinOwner::User, cas).unwrap();
    assert!(user_pin.is_applied());
}

#[test]
fn review_reject_removes_the_pending_file_and_suppresses_the_same_proposal() {
    let fixture = Fixture::new(MemoryScope::Project);
    let store = fixture.store();
    create_user(
        &store,
        MemoryScope::Project,
        "billing_model",
        "subscription",
        "The provider bills by subscription.",
    );
    let intent = fact_intent(
        MemoryScope::Project,
        Some("billing_model"),
        "usage",
        "The provider bills by usage.",
    );
    let pending = pending_id(&admit(&store, model(), intent.clone()));
    let rejected = store
        .review(
            op(),
            user_review(),
            &pending,
            ReviewDecision::Reject {
                reason: Reason::new("not confirmed"),
            },
        )
        .unwrap();
    assert!(
        matches!(rejected, Admission::Rejected { .. }),
        "{rejected:?}"
    );
    assert!(store.list_pending().unwrap().is_empty());
    assert!(matches!(store.read(&pending), Err(StoreError::NotFound(_))));

    let again = admit(&store, model(), intent);
    assert!(
        matches!(
            again,
            Admission::Rejected {
                reason: leveler_memory::v2::RejectReason::Other(ref reason),
            } if reason == "rejected_previously"
        ),
        "{again:?}"
    );
    assert_eq!(effective(&store).len(), 1);
}

#[test]
fn accepting_a_proposal_whose_target_changed_returns_conflict() {
    let fixture = Fixture::new(MemoryScope::Project);
    let store = fixture.store();
    let original = create_user(
        &store,
        MemoryScope::Project,
        "billing_model",
        "subscription",
        "The provider bills by subscription.",
    );
    let original_id = applied_id(&original);
    let proposal = admit(
        &store,
        model(),
        fact_intent(
            MemoryScope::Project,
            Some("billing_model"),
            "usage",
            "The provider bills by usage.",
        ),
    );
    let pending = pending_id(&proposal);

    let read = store.read(&original_id).unwrap();
    let patch = leveler_memory::v2::RecordPatch {
        value: Some("subscription_v2".into()),
        body: Some("The provider bills by subscription, tier two.".into()),
        ..Default::default()
    };
    assert!(
        store
            .update(op(), user(), &original_id, patch, read.cas)
            .unwrap()
            .is_applied()
    );

    let result = store
        .review(op(), user_review(), &pending, ReviewDecision::Accept)
        .unwrap();
    assert!(matches!(result, Admission::Conflict { .. }), "{result:?}");
    assert_eq!(store.list_pending().unwrap().len(), 1);
    assert_eq!(
        store
            .read(&original_id)
            .unwrap()
            .record
            .frontmatter
            .value
            .as_deref(),
        Some("subscription_v2")
    );
}

#[test]
fn a_runtime_user_fact_outside_project_scope_needs_a_user_command() {
    let fixture = Fixture::new(MemoryScope::Global);
    let store = fixture.store();
    let admission = admit(
        &store,
        runtime(),
        fact_intent(MemoryScope::Global, Some("editor"), "vim", "Prefer vim."),
    );
    assert!(matches!(
        admission,
        Admission::Pending {
            reason: PendingReason::ScopeRequiresUserCommand,
            ..
        }
    ));
    assert!(effective(&store).is_empty());
}

#[test]
fn an_external_caller_cannot_write_memory() {
    let fixture = Fixture::new(MemoryScope::Project);
    let store = fixture.store();
    let admission = admit(
        &store,
        Caller::External,
        fact_intent(
            MemoryScope::Project,
            Some("billing_model"),
            "usage",
            "The provider bills by usage.",
        ),
    );
    assert!(matches!(
        admission,
        Admission::Rejected {
            reason: leveler_memory::v2::RejectReason::ExternalCaller,
            ..
        }
    ));
    assert!(effective(&store).is_empty());
}

#[test]
fn migration_cannot_mint_an_explicit_user_decision() {
    let fixture = Fixture::new(MemoryScope::Project);
    let store = fixture.store();
    let mut intent = fact_intent(
        MemoryScope::Project,
        Some("billing_model"),
        "monthly",
        "The provider bills monthly.",
    );
    intent.proposed_authority = MemoryAuthority::UserExplicit;
    let admission = admit(&store, migration(), intent);
    let id = applied_id(&admission);
    assert_eq!(
        store.read(&id).unwrap().record.frontmatter.authority,
        MemoryAuthority::UserStated
    );
}

#[test]
fn an_unresolved_runtime_experience_is_tool_observed() {
    let fixture = Fixture::new(MemoryScope::Project);
    let store = fixture.store();
    let unresolved = admit(
        &store,
        runtime(),
        experience_intent(
            MemoryScope::Project,
            "build",
            "cargo build fails",
            "The build failure is unresolved.",
            false,
        ),
    );
    let id = applied_id(&unresolved);
    assert_eq!(
        store.read(&id).unwrap().record.frontmatter.authority,
        MemoryAuthority::ToolObserved
    );
    assert_eq!(
        store.read(&id).unwrap().record.frontmatter.kind,
        MemoryKind::Experience
    );
}
