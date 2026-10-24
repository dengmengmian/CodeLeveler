//! Service-level actor, authority and lifecycle contracts.
//!
//! These tests drive the same entry points production call sites use. They do
//! not re-test the store's file protocol; they pin who may do what and which
//! authority a write can acquire.

mod memory_v2_common;

use leveler_core::{SessionId, TaskId, ToolCallId, TurnId};
use leveler_memory::v2::{
    Actor, Admission, IdentityRef, MemoryAuthority, MemoryId, MemoryKind, MemoryScope,
    MemoryService, MemoryStatus, MigrationId, PendingReason, PinOwner, ReadRecord, RecordPatch,
    Relation, ReviewDecision, SourceRef, SourceRefKind, StatementKind, StoreError,
    UserInvocationId, WriteIntent,
};
use memory_v2_common::{Fixture, op, source_ref, verbatim_source};
use uuid::Uuid;

fn service(fixture: &Fixture) -> MemoryService {
    MemoryService::open(fixture.roots()).unwrap()
}

fn user() -> Actor {
    Actor::User {
        invocation: UserInvocationId::mint(format!("inv-{}", Uuid::new_v4().simple())),
    }
}

fn model() -> Actor {
    Actor::Model {
        session: SessionId::new("session-1"),
        turn: TurnId::new("turn-1"),
        tool_call: ToolCallId::new(format!("call-{}", Uuid::new_v4().simple())),
    }
}

fn runtime() -> Actor {
    Actor::Runtime {
        task: TaskId::new(format!("task-{}", Uuid::new_v4().simple())),
    }
}

fn fact(scope: MemoryScope, subject_key: Option<&str>, value: &str, body: &str) -> WriteIntent {
    WriteIntent {
        op_id: op(),
        kind: MemoryKind::UserFact,
        scope,
        subject: subject_key.map(str::to_string),
        subject_key: subject_key.map(str::to_string),
        value: Some(value.to_string()),
        title: subject_key.unwrap_or("memory").to_string(),
        body: body.to_string(),
        tags: Vec::new(),
        statement: StatementKind::Assertion,
        relation: Relation::None,
        expires_at: None,
        evidence: Vec::new(),
        experience: None,
        proposed_authority: MemoryAuthority::ModelInferred,
        expected: None,
    }
}

fn apply(admission: Admission) -> (MemoryId, u64) {
    match admission {
        Admission::Applied { id, revision, .. } | Admission::Replayed { id, revision, .. } => {
            (id, revision)
        }
        other => panic!("expected an applied admission, got {other:?}"),
    }
}

fn pending(admission: Admission) -> MemoryId {
    match admission {
        Admission::Pending { id, .. } => id,
        other => panic!("expected a pending admission, got {other:?}"),
    }
}

fn read(service: &MemoryService, id: &MemoryId) -> ReadRecord {
    let actor = user();
    service.read(&actor, id).unwrap()
}

#[test]
fn a_user_command_writes_user_explicit_authority() {
    let fixture = Fixture::new(MemoryScope::Project);
    let service = service(&fixture);
    let (id, revision) = apply(
        service
            .admit(
                &user(),
                fact(
                    MemoryScope::Project,
                    Some("billing_model"),
                    "subscription",
                    "Billing is subscription.",
                ),
            )
            .unwrap(),
    );
    assert_eq!(revision, 1);
    let record = read(&service, &id);
    assert_eq!(
        record.record.frontmatter.authority,
        MemoryAuthority::UserExplicit
    );
    let identities = service.identity_context(24).unwrap();
    assert_eq!(identities.len(), 1);
    assert_eq!(identities[0].id, id.to_string());
}

#[test]
fn a_model_write_cannot_promote_itself_by_claiming_authority() {
    let fixture = Fixture::new(MemoryScope::Project);
    let service = service(&fixture);
    let mut intent = fact(
        MemoryScope::Project,
        Some("billing_model"),
        "usage",
        "Billing is usage based.",
    );
    intent.proposed_authority = MemoryAuthority::UserExplicit;
    let (id, _) = apply(service.admit(&model(), intent).unwrap());
    assert_eq!(
        read(&service, &id).record.frontmatter.authority,
        MemoryAuthority::ModelInferred
    );
}

#[test]
fn a_model_cannot_forge_user_provenance() {
    let fixture = Fixture::new(MemoryScope::Project);
    let service = service(&fixture);
    let mut intent = fact(
        MemoryScope::Project,
        Some("billing_model"),
        "usage",
        "Billing.",
    );
    let mut forged = source_ref();
    forged.kind = SourceRefKind::UserCommand;
    intent.evidence = vec![forged];
    let error = service.admit(&model(), intent).unwrap_err();
    assert!(matches!(error, StoreError::Invalid(_)), "{error:?}");
    assert!(service.effective_active().unwrap().is_empty());
}

#[test]
fn a_model_cannot_claim_a_user_turn_span() {
    let fixture = Fixture::new(MemoryScope::Project);
    let service = service(&fixture);
    let mut intent = fact(
        MemoryScope::Project,
        Some("billing_model"),
        "usage",
        "Billing.",
    );
    intent.evidence = vec![source_ref()];
    let error = service.admit(&model(), intent).unwrap_err();
    assert!(matches!(error, StoreError::Invalid(_)), "{error:?}");
}

#[test]
fn a_model_fact_outside_project_scope_waits_for_a_person() {
    let fixture = Fixture::new(MemoryScope::Global);
    let service = service(&fixture);
    let id = pending(
        service
            .admit(
                &model(),
                fact(
                    MemoryScope::Global,
                    Some("editor"),
                    "vim",
                    "The editor is vim.",
                ),
            )
            .unwrap(),
    );
    assert!(service.effective_active().unwrap().is_empty());
    assert_eq!(service.pending(&user()).unwrap().len(), 1);
    assert_eq!(service.pending(&user()).unwrap()[0].frontmatter.id, id);
}

#[test]
fn a_model_cannot_replace_a_protected_user_fact() {
    let fixture = Fixture::new(MemoryScope::Project);
    let service = service(&fixture);
    let (id, _) = apply(
        service
            .admit(
                &user(),
                fact(
                    MemoryScope::Project,
                    Some("billing_model"),
                    "subscription",
                    "Billing is subscription.",
                ),
            )
            .unwrap(),
    );
    let id2 = pending(
        service
            .admit(
                &model(),
                fact(
                    MemoryScope::Project,
                    Some("billing_model"),
                    "usage",
                    "Billing is usage based.",
                ),
            )
            .unwrap(),
    );
    assert_ne!(id, id2);
    let active = service.effective_active().unwrap();
    assert_eq!(active.len(), 1);
    assert_eq!(
        active[0].record.frontmatter.value.as_deref(),
        Some("subscription")
    );
}

#[test]
fn a_runtime_user_fact_requires_an_exact_user_span() {
    let fixture = Fixture::new(MemoryScope::Project);
    let service = service(&fixture);
    let error = service
        .admit(
            &runtime(),
            fact(
                MemoryScope::Project,
                Some("billing_model"),
                "usage",
                "Billing is usage.",
            ),
        )
        .unwrap_err();
    assert!(matches!(error, StoreError::Invalid(_)), "{error:?}");

    let mut intent = fact(
        MemoryScope::Project,
        Some("billing_model"),
        "usage",
        "Billing is usage.",
    );
    intent.evidence = vec![verbatim_source("Billing is usage.")];
    let (id, _) = apply(service.admit(&runtime(), intent).unwrap());
    assert_eq!(
        read(&service, &id).record.frontmatter.authority,
        MemoryAuthority::UserStated
    );
}

/// A span that exists but does not contain the body proves the opposite of
/// what the writer claims: the runtime is storing its own wording. It waits
/// for a person instead of speaking as one.
#[test]
fn a_runtime_user_fact_is_parked_when_its_span_does_not_contain_it() {
    let fixture = Fixture::new(MemoryScope::Project);
    let service = service(&fixture);
    let mut intent = fact(
        MemoryScope::Project,
        Some("billing_model"),
        "usage",
        "Billing is usage.",
    );
    intent.evidence = vec![source_ref()];
    let parked = service.admit(&runtime(), intent).unwrap();
    assert!(
        matches!(
            parked,
            Admission::Pending {
                reason: PendingReason::UnverifiedUserStatement,
                ..
            }
        ),
        "{parked:?}"
    );
    assert!(service.effective_active().unwrap().is_empty());
}

#[test]
fn the_same_op_id_replays_and_a_changed_request_is_rejected() {
    let fixture = Fixture::new(MemoryScope::Project);
    let service = service(&fixture);
    let actor = user();
    let intent = fact(
        MemoryScope::Project,
        Some("billing_model"),
        "subscription",
        "Billing is subscription.",
    );
    let op_id = intent.op_id.clone();
    let (id, revision) = apply(service.admit(&actor, intent.clone()).unwrap());

    let replay = service.admit(&actor, intent.clone()).unwrap();
    assert!(matches!(
        replay,
        Admission::Replayed { id: replay_id, revision: 1, .. } if replay_id == id
    ));

    let mut changed = intent;
    changed.value = Some("usage".into());
    changed.op_id = op_id;
    let error = service.admit(&actor, changed).unwrap_err();
    assert!(matches!(error, StoreError::OpIdReuse(_)), "{error:?}");
    assert_eq!(revision, 1);
    assert_eq!(service.effective_active().unwrap().len(), 1);
}

#[test]
fn two_facts_in_one_turn_are_two_writes() {
    let fixture = Fixture::new(MemoryScope::Project);
    let service = service(&fixture);
    let actor = model();
    let first = service
        .admit(
            &actor,
            fact(
                MemoryScope::Project,
                Some("billing_model"),
                "usage",
                "Billing is usage.",
            ),
        )
        .unwrap();
    let second = service
        .admit(
            &actor,
            fact(
                MemoryScope::Project,
                Some("deploy_target"),
                "fly",
                "Deploys go to fly.",
            ),
        )
        .unwrap();
    assert!(first.is_applied());
    assert!(second.is_applied());
    assert_eq!(service.effective_active().unwrap().len(), 2);
}

#[test]
fn a_user_review_activates_pending_with_user_approved_authority() {
    let fixture = Fixture::new(MemoryScope::Project);
    let service = service(&fixture);
    let (_user_id, _) = apply(
        service
            .admit(
                &user(),
                fact(
                    MemoryScope::Project,
                    Some("editor"),
                    "emacs",
                    "The editor is emacs.",
                ),
            )
            .unwrap(),
    );
    let queued = pending(
        service
            .admit(
                &model(),
                fact(
                    MemoryScope::Project,
                    Some("editor"),
                    "vim",
                    "The editor is vim.",
                ),
            )
            .unwrap(),
    );
    let admission = service
        .review(&user(), &queued, ReviewDecision::Accept)
        .unwrap();
    let (accepted, _) = apply(admission);
    assert_eq!(accepted, queued);
    let active = service.effective_active().unwrap();
    assert_eq!(active.len(), 1);
    assert_eq!(active[0].record.frontmatter.id, queued);
    assert_eq!(active[0].effective_authority, MemoryAuthority::UserApproved);
}

#[test]
fn a_user_rejection_deletes_the_pending_record_and_suppresses_the_signal() {
    let fixture = Fixture::new(MemoryScope::Project);
    let service = service(&fixture);
    let _ = apply(
        service
            .admit(
                &user(),
                fact(
                    MemoryScope::Project,
                    Some("editor"),
                    "emacs",
                    "The editor is emacs.",
                ),
            )
            .unwrap(),
    );
    let queued = pending(
        service
            .admit(
                &model(),
                fact(
                    MemoryScope::Project,
                    Some("editor"),
                    "vim",
                    "The editor is vim.",
                ),
            )
            .unwrap(),
    );
    let admission = service
        .review(
            &user(),
            &queued,
            ReviewDecision::Reject {
                reason: leveler_memory::v2::Reason::new("not this project"),
            },
        )
        .unwrap();
    assert!(matches!(admission, Admission::Rejected { .. }));
    assert!(service.pending(&user()).unwrap().is_empty());
    assert!(matches!(
        service.read(&user(), &queued),
        Err(StoreError::NotFound(_))
    ));
}

#[test]
fn only_a_user_can_review() {
    let fixture = Fixture::new(MemoryScope::Project);
    let service = service(&fixture);
    let _ = apply(
        service
            .admit(
                &user(),
                fact(
                    MemoryScope::Project,
                    Some("editor"),
                    "emacs",
                    "The editor is emacs.",
                ),
            )
            .unwrap(),
    );
    let queued = pending(
        service
            .admit(
                &model(),
                fact(
                    MemoryScope::Project,
                    Some("editor"),
                    "vim",
                    "The editor is vim.",
                ),
            )
            .unwrap(),
    );
    let error = service
        .review(&model(), &queued, ReviewDecision::Accept)
        .unwrap_err();
    assert!(matches!(error, StoreError::ForbiddenActor(_)), "{error:?}");
    let error = service.pending(&model()).unwrap_err();
    assert!(matches!(error, StoreError::ForbiddenActor(_)), "{error:?}");
}

#[test]
fn a_user_pin_is_user_owned_and_a_model_cannot_pin_a_user_fact() {
    let fixture = Fixture::new(MemoryScope::Project);
    let service = service(&fixture);
    let actor = user();
    let (id, _) = apply(
        service
            .admit(
                &actor,
                fact(
                    MemoryScope::Project,
                    Some("billing_model"),
                    "subscription",
                    "Billing is subscription.",
                ),
            )
            .unwrap(),
    );
    let cas = read(&service, &id).cas;
    service.pin(&actor, op(), &id, cas).unwrap();
    assert_eq!(
        read(&service, &id).record.frontmatter.pinned_by,
        Some(PinOwner::User)
    );

    let cas = read(&service, &id).cas;
    let queued = pending(service.pin(&model(), op(), &id, cas).unwrap());
    assert!(
        service
            .pending(&user())
            .unwrap()
            .iter()
            .any(|record| record.frontmatter.id == queued)
    );
}

#[test]
fn a_model_forget_of_a_user_fact_waits_and_a_user_forget_tombstones() {
    let fixture = Fixture::new(MemoryScope::Project);
    let service = service(&fixture);
    let actor = user();
    let (id, _) = apply(
        service
            .admit(
                &actor,
                fact(
                    MemoryScope::Project,
                    Some("billing_model"),
                    "subscription",
                    "Billing is subscription.",
                ),
            )
            .unwrap(),
    );
    let cas = read(&service, &id).cas;
    let outcome = service.forget(&model(), op(), &id, cas).unwrap();
    assert!(matches!(
        outcome,
        leveler_memory::v2::ForgetOutcome::Pending(_)
    ));
    assert!(service.read(&user(), &id).is_ok());

    let cas = read(&service, &id).cas;
    let outcome = service.forget(&actor, op(), &id, cas).unwrap();
    assert!(matches!(
        outcome,
        leveler_memory::v2::ForgetOutcome::Forgotten(_)
    ));
    assert!(matches!(
        service.read(&actor, &id),
        Err(StoreError::Forgotten(_))
    ));
    assert!(service.effective_active().unwrap().is_empty());
}

#[test]
fn a_stale_cas_conflicts_instead_of_overwriting() {
    let fixture = Fixture::new(MemoryScope::Project);
    let service = service(&fixture);
    let actor = user();
    let (id, _) = apply(
        service
            .admit(
                &actor,
                fact(
                    MemoryScope::Project,
                    Some("billing_model"),
                    "subscription",
                    "Billing is subscription.",
                ),
            )
            .unwrap(),
    );
    let stale = read(&service, &id).cas;
    let patch = RecordPatch {
        value: Some("usage".into()),
        body: Some("Billing is usage.".into()),
        ..Default::default()
    };
    service
        .update(&actor, op(), &id, patch.clone(), stale.clone())
        .unwrap();
    let conflict = service.update(&actor, op(), &id, patch, stale).unwrap();
    assert!(matches!(conflict, Admission::Conflict { .. }));
    assert_eq!(
        read(&service, &id).record.frontmatter.value.as_deref(),
        Some("usage")
    );
}

#[test]
fn external_edits_never_become_current_truth() {
    let fixture = Fixture::new(MemoryScope::Project);
    let service = service(&fixture);
    let key = service.effective_active().unwrap();
    assert!(key.is_empty());
    let (id, _) = apply(
        service
            .admit(
                &user(),
                fact(
                    MemoryScope::Project,
                    Some("billing_model"),
                    "subscription",
                    "Billing is subscription.",
                ),
            )
            .unwrap(),
    );
    let path = fixture.current_file(&id);
    let mut text = std::fs::read_to_string(&path).unwrap();
    text.push_str("\nEdited outside the store.\n");
    std::fs::write(&path, text).unwrap();
    assert!(service.effective_active().unwrap().is_empty());
    let record = service.read(&user(), &id).unwrap();
    assert_eq!(
        record.trust,
        leveler_memory::v2::TrustSummary::ExternalUnverified
    );
}

#[test]
fn identity_context_is_bounded_and_user_decisions_come_first() {
    let fixture = Fixture::new(MemoryScope::Project);
    let service = service(&fixture);
    let actor = user();
    let mut ids = Vec::new();
    for index in 0..30 {
        let key = format!("subject_{index}");
        let (id, _) = apply(
            service
                .admit(&actor, fact(MemoryScope::Project, Some(&key), "v", "body"))
                .unwrap(),
        );
        ids.push(id);
    }
    let pinned = &ids[10];
    let cas = read(&service, pinned).cas;
    service.pin(&actor, op(), pinned, cas).unwrap();

    let context: Vec<IdentityRef> = service.identity_context(100).unwrap();
    assert_eq!(context.len(), 24);
    assert_eq!(context[0].id, pinned.to_string());
    assert!(
        context
            .iter()
            .all(|entry| entry.kind == MemoryKind::UserFact)
    );
}

#[test]
fn a_replaces_target_must_be_a_real_current_record() {
    let fixture = Fixture::new(MemoryScope::Project);
    let service = service(&fixture);
    let missing = MemoryId::parse("mem_does_not_exist").unwrap();
    let mut intent = fact(
        MemoryScope::Project,
        Some("billing_model"),
        "usage",
        "Billing is usage.",
    );
    intent.relation = Relation::Replaces { target: missing };
    let admission = service.admit(&user(), intent).unwrap();
    assert!(matches!(admission, Admission::Rejected { .. }));
}

#[test]
fn a_scope_mismatch_is_rejected() {
    let fixture = Fixture::new(MemoryScope::Project);
    let service = service(&fixture);
    let admission = service
        .admit(
            &user(),
            fact(
                MemoryScope::Global,
                Some("editor"),
                "vim",
                "The editor is vim.",
            ),
        )
        .unwrap();
    assert!(matches!(admission, Admission::Rejected { .. }));
    assert!(service.effective_active().unwrap().is_empty());
}

#[test]
fn expiry_retires_the_record_from_current_truth() {
    let fixture = Fixture::new(MemoryScope::Project);
    let service = service(&fixture);
    let mut intent = fact(
        MemoryScope::Project,
        Some("temporary"),
        "v",
        "Temporary fact.",
    );
    intent.expires_at = Some(chrono::Utc::now() + chrono::Duration::hours(1));
    let (id, _) = apply(service.admit(&user(), intent).unwrap());
    let expired = service
        .expire_due(chrono::Utc::now() + chrono::Duration::hours(2))
        .unwrap();
    assert_eq!(expired, vec![id.clone()]);
    assert!(service.effective_active().unwrap().is_empty());
    assert_eq!(
        service.history(&id, 1).unwrap().frontmatter.status,
        MemoryStatus::Active
    );
}

#[test]
fn an_already_expired_fact_is_rejected_instead_of_written_unreadable() {
    let fixture = Fixture::new(MemoryScope::Project);
    let service = service(&fixture);
    let mut intent = fact(
        MemoryScope::Project,
        Some("temporary"),
        "v",
        "Temporary fact.",
    );
    intent.expires_at = Some(chrono::Utc::now() - chrono::Duration::seconds(1));
    let admission = service.admit(&user(), intent).unwrap();
    assert!(matches!(admission, Admission::Rejected { .. }));
    assert!(service.effective_active().unwrap().is_empty());
}

#[test]
fn a_migration_record_keeps_its_legacy_provenance_but_caps_authority() {
    let fixture = Fixture::new(MemoryScope::Project);
    let service = service(&fixture);
    let actor = Actor::Migration {
        run: MigrationId::mint("run-1"),
    };
    let mut intent = fact(
        MemoryScope::Project,
        Some("legacy_fact"),
        "v",
        "Migrated fact.",
    );
    intent.proposed_authority = MemoryAuthority::UserExplicit;
    intent.evidence = vec![SourceRef {
        kind: SourceRefKind::Migration,
        legacy_store: Some("legacy".into()),
        legacy_id: Some("old-id".into()),
        ..source_ref()
    }];
    let (id, _) = apply(service.admit(&actor, intent).unwrap());
    let record = read(&service, &id);
    assert_eq!(
        record.record.frontmatter.authority,
        MemoryAuthority::ModelInferred,
        "an imported sentence is not evidence of what the user said"
    );
    assert!(
        record
            .record
            .frontmatter
            .source_refs
            .iter()
            .any(|source| source.legacy_id.as_deref() == Some("old-id")),
        "the legacy provenance is still recorded: {record:?}"
    );
}

#[test]
fn review_accept_of_a_stale_pending_conflicts_without_overwriting() {
    let fixture = Fixture::new(MemoryScope::Project);
    let service = service(&fixture);
    let actor = user();
    let (id, _) = apply(
        service
            .admit(
                &actor,
                fact(
                    MemoryScope::Project,
                    Some("editor"),
                    "emacs",
                    "The editor is emacs.",
                ),
            )
            .unwrap(),
    );
    let queued = pending(
        service
            .admit(
                &model(),
                fact(
                    MemoryScope::Project,
                    Some("editor"),
                    "vim",
                    "The editor is vim.",
                ),
            )
            .unwrap(),
    );
    // Move the target after the proposal was staged; acceptance must notice.
    let cas = read(&service, &id).cas;
    service
        .update(
            &actor,
            op(),
            &id,
            RecordPatch {
                value: Some("nano".into()),
                body: Some("The editor is nano.".into()),
                ..Default::default()
            },
            cas,
        )
        .unwrap();
    let conflict = service
        .review(&actor, &queued, ReviewDecision::Accept)
        .unwrap();
    assert!(matches!(conflict, Admission::Conflict { .. }));
    assert_eq!(
        service.effective_active().unwrap()[0]
            .record
            .frontmatter
            .value
            .as_deref(),
        Some("nano")
    );
}
