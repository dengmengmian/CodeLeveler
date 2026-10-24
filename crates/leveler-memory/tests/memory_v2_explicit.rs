//! Explicit unstructured memory: a trusted user command without a subject is
//! stored under its stable memory id, never under an invented subject.
//!
//! These tests drive the same service facade the app, TUI and tools call. They
//! pin the identity boundaries: an explicit record is addressable only by id,
//! automatic writers still need a structured subject, and a direct write is
//! not duplicated by the same turn's extractor.

mod memory_v2_common;

use leveler_core::{SessionId, TaskId, ToolCallId, TurnId};
use leveler_memory::v2::{
    Actor, MemoryAuthority, MemoryKind, MemoryScope, SourceRefKind, UserInvocationId,
};
use leveler_memory::{
    CandidateKind, CandidateSource, MemoryCandidate, MemoryLanes, MemoryRoots, MemoryService,
    RememberRequest, ReviewRequest, ServiceScope, UpdateRequest, WriteOutcome,
};
use memory_v2_common::Fixture;

fn open_service(fixture: &Fixture) -> MemoryService {
    MemoryService::new(MemoryRoots::new().with_project(fixture.roots()))
}

fn user(tag: &str) -> Actor {
    Actor::User {
        invocation: UserInvocationId::mint(format!("test:{tag}")),
    }
}

fn model() -> Actor {
    Actor::Model {
        session: SessionId::new("session-1"),
        turn: TurnId::new("turn-1"),
        tool_call: ToolCallId::new(format!("call-{}", uuid::Uuid::new_v4().simple())),
    }
}

fn runtime() -> Actor {
    Actor::Runtime {
        task: TaskId::new(format!("task-{}", uuid::Uuid::new_v4().simple())),
    }
}

fn direct(service: &MemoryService, actor: &Actor, op: &str, body: &str) -> WriteOutcome {
    service
        .remember(
            actor,
            RememberRequest {
                op_id: op.to_string(),
                scope: ServiceScope::Project,
                title: leveler_memory::title_from_body(body),
                body: body.to_string(),
                kind: "preference".to_string(),
                subject: None,
                value: None,
                tags: Vec::new(),
                evidence: Some(body.to_string()),
                expires_at: None,
                replaces_id: None,
            },
        )
        .unwrap()
}

fn active(service: &MemoryService) -> Vec<leveler_memory::MemoryView> {
    service
        .listing_for_user(ServiceScope::Project, MemoryLanes::ALL)
        .unwrap()
}

fn applied(outcome: WriteOutcome) -> (String, u64, bool) {
    match outcome {
        WriteOutcome::Applied {
            id,
            revision,
            replayed,
            ..
        } => (
            id,
            revision.expect("markdown writes carry a revision"),
            replayed,
        ),
        other => panic!("expected an applied write, got {other:?}"),
    }
}

/// Without a subject, a user's own `/remember` becomes one record whose
/// identity is its stable memory id. The file carries the trusted invocation.
#[test]
fn a_direct_remember_without_subject_is_saved_and_reloads() {
    let fixture = Fixture::new(MemoryScope::Project);
    let service = open_service(&fixture);
    let (id, revision, replayed) = applied(direct(
        &service,
        &user("cmd-1"),
        "direct:cmd-1",
        "这个项目使用 pnpm",
    ));
    assert_eq!(revision, 1);
    assert!(!replayed);

    let entries = active(&service);
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].id, id);
    assert!(entries[0].subject_key.is_none());
    assert_eq!(entries[0].authority, "user_explicit");

    let path = fixture.current.join(format!("{id}.md"));
    let record = leveler_memory::v2::frontmatter::parse(&std::fs::read(&path).unwrap()).unwrap();
    assert_eq!(record.frontmatter.kind, MemoryKind::UserFact);
    assert!(record.frontmatter.subject_key.is_none());
    assert_eq!(record.frontmatter.authority, MemoryAuthority::UserExplicit);
    assert!(
        record.frontmatter.source_refs.iter().any(|source| {
            source.kind == SourceRefKind::UserCommand
                && source
                    .extra
                    .values()
                    .any(|value| value.as_str() == Some("test:cmd-1"))
        }),
        "the record must name the trusted invocation it came from: {:?}",
        record.frontmatter.source_refs
    );

    // A new process must see the same identity and revision.
    drop(service);
    let reopened = open_service(&fixture);
    let entries = active(&reopened);
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].id, id);
    assert_eq!(entries[0].revision, Some(1));
}

/// One user command retried with the same invocation replays; it may not
/// become a second record.
#[test]
fn retrying_one_direct_command_produces_one_record() {
    let fixture = Fixture::new(MemoryScope::Project);
    let service = open_service(&fixture);
    let (first, _, _) = applied(direct(
        &service,
        &user("cmd-1"),
        "direct:cmd-1",
        "使用 pnpm",
    ));
    let (second, revision, replayed) = applied(direct(
        &service,
        &user("cmd-1"),
        "direct:cmd-1",
        "使用 pnpm",
    ));
    assert_eq!(first, second);
    assert_eq!(revision, 1);
    assert!(replayed, "a retried command must replay, not apply twice");
    assert_eq!(active(&service).len(), 1);
}

/// Two independent user commands are two operations, even when their text is
/// identical. Nothing here dedupes memory by content.
#[test]
fn two_independent_direct_commands_are_both_stored() {
    let fixture = Fixture::new(MemoryScope::Project);
    let service = open_service(&fixture);
    let (first, _, _) = applied(direct(
        &service,
        &user("cmd-1"),
        "direct:cmd-1",
        "使用 pnpm",
    ));
    let (second, _, replayed) = applied(direct(
        &service,
        &user("cmd-2"),
        "direct:cmd-2",
        "使用 pnpm",
    ));
    assert_ne!(first, second);
    assert!(!replayed);
    assert_eq!(active(&service).len(), 2);
}

/// An automatic writer without a structured subject is still refused. The
/// explicit path must not widen the extractor's admission contract.
#[test]
fn automatic_writes_without_subject_are_still_rejected() {
    let fixture = Fixture::new(MemoryScope::Project);
    let service = open_service(&fixture);
    // The production extraction path goes through `admit_candidate`, whose
    // model meaning requirement is checked before any store write.
    let mut candidate = MemoryCandidate::new(
        "Package manager",
        "这个项目使用 pnpm",
        CandidateKind::Preference,
        None,
        CandidateSource::SystemInferred,
        Vec::new(),
    )
    .unwrap();
    candidate.value = Some("pnpm".into());
    candidate.evidence = Some("这个项目使用 pnpm".into());
    let outcome = service.admit_candidate(&runtime(), ServiceScope::Project, &candidate, "inbox-1");
    assert!(
        outcome.is_err(),
        "an automatic candidate without a subject must be refused: {outcome:?}"
    );
    // The store boundary itself also refuses an automatic keyless fact, so a
    // future caller cannot reach the keyless branch by another route.
    let admission = memory_v2_common::admit(
        &fixture.store(),
        memory_v2_common::model(),
        memory_v2_common::fact_intent(MemoryScope::Project, None, "pnpm", "这个项目使用 pnpm"),
    );
    assert!(
        matches!(admission, leveler_memory::v2::Admission::Rejected { ref reason } if reason.as_str() == "missing_subject_key"),
        "{admission:?}"
    );
    assert!(active(&service).is_empty());
}

/// The same turn's extractor must not create a second active record beside the
/// direct command that already owns the exact source span, while a different
/// fact from the same turn is still admitted.
#[test]
fn the_same_turn_extractor_does_not_duplicate_a_direct_write() {
    let fixture = Fixture::new(MemoryScope::Project);
    let service = open_service(&fixture);
    let (direct_id, _, _) = applied(direct(
        &service,
        &user("cmd-1"),
        "direct:cmd-1",
        "这个项目使用 pnpm",
    ));

    let mut duplicated = MemoryCandidate::new(
        "Package manager",
        "这个项目使用 pnpm",
        CandidateKind::Preference,
        Some("package_manager".into()),
        CandidateSource::SystemInferred,
        Vec::new(),
    )
    .unwrap();
    duplicated.value = Some("pnpm".into());
    duplicated.evidence = Some("这个项目使用 pnpm".into());
    let outcome = service
        .admit_candidate(&runtime(), ServiceScope::Project, &duplicated, "inbox-1")
        .unwrap();
    assert!(
        matches!(outcome, WriteOutcome::Skipped { ref reason, .. } if reason == "direct_command_owned"),
        "the direct write owns this span: {outcome:?}"
    );
    assert_eq!(active(&service).len(), 1);
    assert_eq!(active(&service)[0].id, direct_id);

    let mut independent = MemoryCandidate::new(
        "Test runner",
        "测试使用 nextest 运行",
        CandidateKind::Free,
        Some("test_runner".into()),
        CandidateSource::SystemInferred,
        Vec::new(),
    )
    .unwrap();
    independent.value = Some("nextest".into());
    independent.evidence = Some("测试使用 nextest 运行".into());
    let outcome = service
        .admit_candidate(&runtime(), ServiceScope::Project, &independent, "inbox-1")
        .unwrap();
    assert!(
        matches!(outcome, WriteOutcome::Applied { .. }),
        "an independent fact in the same turn is still extracted: {outcome:?}"
    );
    assert_eq!(active(&service).len(), 2);
}

/// A model may not edit a protected explicit memory directly; the change is
/// parked for the user instead of applied.
#[test]
fn a_model_update_to_an_explicit_memory_is_pending() {
    let fixture = Fixture::new(MemoryScope::Project);
    let service = open_service(&fixture);
    let (id, revision, _) = applied(direct(
        &service,
        &user("cmd-1"),
        "direct:cmd-1",
        "这个项目使用 pnpm",
    ));
    let outcome = service
        .update(
            &model(),
            ServiceScope::Project,
            &id,
            revision,
            UpdateRequest {
                title: None,
                subject: None,
                body: Some("这个项目使用 npm".into()),
                value: None,
                tags: None,
                expires_at: None,
            },
        )
        .unwrap();
    assert!(
        matches!(outcome, WriteOutcome::Pending { .. }),
        "{outcome:?}"
    );
    let entries = active(&service);
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].body, "这个项目使用 pnpm");
    assert_eq!(
        service
            .pending(&user("reader"), ServiceScope::Project)
            .unwrap()
            .len(),
        1
    );
}

/// A direct memory can be corrected by id without inventing a subject.
#[test]
fn a_direct_memory_is_updated_by_id() {
    let fixture = Fixture::new(MemoryScope::Project);
    let service = open_service(&fixture);
    let (id, revision, _) = applied(direct(
        &service,
        &user("cmd-1"),
        "direct:cmd-1",
        "这个项目使用 pnpm",
    ));
    let (updated, next, _) = applied(
        service
            .update(
                &user("cmd-2"),
                ServiceScope::Project,
                &id,
                revision,
                UpdateRequest {
                    title: Some("包管理器".into()),
                    subject: None,
                    body: Some("这个项目使用 pnpm 8".into()),
                    value: Some("pnpm@8".into()),
                    tags: None,
                    expires_at: None,
                },
            )
            .unwrap(),
    );
    assert_eq!(updated, id, "an update keeps the record identity");
    assert_eq!(next, revision + 1);
    let entries = active(&service);
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].body, "这个项目使用 pnpm 8");
    assert_eq!(entries[0].value.as_deref(), Some("pnpm@8"));
    assert!(entries[0].subject_key.is_none());
    let raw = std::fs::read_to_string(
        fixture.current_file(&leveler_memory::v2::MemoryId::parse(&id).unwrap()),
    )
    .unwrap();
    assert!(
        raw.contains("invocation: test:cmd-2"),
        "the record keeps the trusted operation that produced the update: {raw}"
    );
}

/// A credential-shaped direct write is refused before anything is rendered.
#[test]
fn a_secret_in_a_direct_write_is_not_stored() {
    let fixture = Fixture::new(MemoryScope::Project);
    let service = open_service(&fixture);
    let outcome = service.remember(
        &user("cmd-secret"),
        RememberRequest {
            op_id: "direct:secret".into(),
            scope: ServiceScope::Project,
            title: "API key".into(),
            body: "api_key=sk-abcdef1234567890abcdef".into(),
            kind: "preference".into(),
            subject: None,
            value: None,
            tags: Vec::new(),
            evidence: None,
            expires_at: None,
            replaces_id: None,
        },
    );
    assert!(outcome.is_err(), "a secret write must fail: {outcome:?}");
    assert!(active(&service).is_empty());
    let mut leaked = Vec::new();
    collect_files(fixture.dir(), &mut leaked);
    for path in leaked {
        let bytes = std::fs::read(&path).unwrap_or_default();
        let text = String::from_utf8_lossy(&bytes);
        assert!(
            !text.contains("sk-abcdef1234567890abcdef"),
            "the refused secret must not survive on disk: {}",
            path.display()
        );
    }
}

fn collect_files(dir: &std::path::Path, out: &mut Vec<std::path::PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect_files(&path, out);
        } else {
            out.push(path);
        }
    }
}

/// A pending accept is a user review, and only a user review. The domain
/// authority never comes from a model or a CLI argument.
#[test]
fn only_a_user_review_may_accept_pending_memory() {
    let fixture = Fixture::new(MemoryScope::Project);
    let service = open_service(&fixture);
    // Produce a pending record through a model update of a protected fact.
    let (id, revision, _) = applied(direct(
        &service,
        &user("cmd-1"),
        "direct:cmd-1",
        "这个项目使用 pnpm",
    ));
    service
        .update(
            &model(),
            ServiceScope::Project,
            &id,
            revision,
            UpdateRequest {
                title: None,
                subject: None,
                body: Some("这个项目使用 npm".into()),
                value: None,
                tags: None,
                expires_at: None,
            },
        )
        .unwrap();
    let pending = service
        .pending(&user("reader"), ServiceScope::Project)
        .unwrap();
    let pending_id = pending[0].id.clone();
    let denied = service.review(
        &runtime(),
        ServiceScope::Project,
        &pending_id,
        ReviewRequest::Accept,
    );
    assert!(denied.is_err(), "a non-user actor must not accept");
}

fn keyed(
    service: &MemoryService,
    actor: &Actor,
    op: &str,
    subject: &str,
    body: &str,
) -> WriteOutcome {
    service
        .remember(
            actor,
            RememberRequest {
                op_id: op.to_string(),
                scope: ServiceScope::Project,
                title: subject.to_string(),
                body: body.to_string(),
                kind: "preference".to_string(),
                subject: Some(subject.to_string()),
                value: None,
                tags: Vec::new(),
                evidence: Some(body.to_string()),
                expires_at: None,
                replaces_id: None,
            },
        )
        .unwrap()
}

fn attach_subject(
    service: &MemoryService,
    actor: &Actor,
    id: &str,
    revision: u64,
    subject: &str,
) -> WriteOutcome {
    service
        .update(
            actor,
            ServiceScope::Project,
            id,
            revision,
            UpdateRequest {
                subject: Some(subject.to_string()),
                ..UpdateRequest::default()
            },
        )
        .unwrap()
}

/// A person may give their own unstructured memory a structured identity by
/// exact id. The record keeps its id; only the identity changes.
#[test]
fn a_user_can_give_an_explicit_memory_a_structured_subject() {
    let fixture = Fixture::new(MemoryScope::Project);
    let service = open_service(&fixture);
    let (id, revision, _) = applied(direct(
        &service,
        &user("cmd-1"),
        "direct:cmd-1",
        "这个项目使用 pnpm",
    ));
    let (converted, next, _) = applied(attach_subject(
        &service,
        &user("cmd-2"),
        &id,
        revision,
        "package_manager",
    ));
    assert_eq!(converted, id);
    assert_eq!(next, revision + 1);
    let entries = active(&service);
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].id, id);
    assert_eq!(entries[0].subject_key.as_deref(), Some("k-package-manager"));
}

/// A model-supplied subject is a label; the stored identity is the canonical
/// semantic key, so the model path and the automatic extractor agree on which
/// facts are the same. Without this, one spelling is a second active record.
#[test]
fn a_model_subject_uses_the_canonical_identity_key() {
    let fixture = Fixture::new(MemoryScope::Project);
    let service = open_service(&fixture);
    let outcome = service
        .remember(
            &model(),
            RememberRequest {
                op_id: "model:1".to_string(),
                scope: ServiceScope::Project,
                title: "发布使用蓝色通道部署".to_string(),
                body: "发布使用蓝色通道部署。".to_string(),
                kind: "preference".to_string(),
                subject: Some("deploy.channel".to_string()),
                value: None,
                tags: Vec::new(),
                evidence: None,
                expires_at: None,
                replaces_id: None,
            },
        )
        .unwrap();
    applied(outcome);
    let entries = active(&service);
    assert_eq!(entries.len(), 1);
    assert_eq!(
        entries[0].subject_key.as_deref(),
        Some("k-deploy-channel"),
        "the model's label must map to the extractor's canonical key"
    );
}

/// The model may suggest an identity, but the conversion is parked for the
/// person; the explicit record stays keyless in the meantime.
#[test]
fn a_model_identity_suggestion_waits_for_the_user() {
    let fixture = Fixture::new(MemoryScope::Project);
    let service = open_service(&fixture);
    let (id, revision, _) = applied(direct(
        &service,
        &user("cmd-1"),
        "direct:cmd-1",
        "这个项目使用 pnpm",
    ));
    let outcome = attach_subject(&service, &model(), &id, revision, "package_manager");
    assert!(
        matches!(outcome, WriteOutcome::Pending { .. }),
        "{outcome:?}"
    );
    let entries = active(&service);
    assert_eq!(entries.len(), 1);
    assert!(entries[0].subject_key.is_none());
    assert_eq!(
        service
            .pending(&user("reader"), ServiceScope::Project)
            .unwrap()
            .len(),
        1
    );
}

/// When the subject is already taken, a conversion waits for review, and an
/// accept whose content is not structurally identical is refused instead of
/// merging two different facts.
#[test]
fn a_subject_already_in_use_waits_and_a_forced_merge_is_refused() {
    let fixture = Fixture::new(MemoryScope::Project);
    let service = open_service(&fixture);
    let (structured, _, _) = applied(keyed(
        &service,
        &user("cmd-structured"),
        "keyed:1",
        "package_manager",
        "pnpm 是包管理器",
    ));
    let (explicit, revision, _) = applied(direct(
        &service,
        &user("cmd-1"),
        "direct:cmd-1",
        "这个项目使用 pnpm",
    ));
    let outcome = attach_subject(
        &service,
        &user("cmd-2"),
        &explicit,
        revision,
        "package_manager",
    );
    let WriteOutcome::Pending { id, reason, .. } = outcome else {
        panic!("a taken subject must wait for review: {outcome:?}");
    };
    assert_eq!(id, explicit);
    assert!(
        reason.starts_with("subject_key_in_use:"),
        "the pending reason names the collision: {reason}"
    );

    let denied = service.review(
        &user("reviewer"),
        ServiceScope::Project,
        &explicit,
        ReviewRequest::Accept,
    );
    assert!(
        denied.is_err(),
        "different content must not merge into one subject: {denied:?}"
    );
    assert_eq!(
        service
            .pending(&user("reader"), ServiceScope::Project)
            .unwrap()
            .len(),
        1,
        "a refused accept leaves the proposal reviewable"
    );
    let keyed_active = active(&service)
        .into_iter()
        .filter(|entry| entry.subject_key.as_deref() == Some("k-package-manager"))
        .count();
    assert_eq!(keyed_active, 1, "no second Active under the same subject");
    let _ = structured;
}

/// Structural equality is enough to converge: the accepted conversion
/// retires the identical structured record instead of leaving two Actives.
#[test]
fn accepting_an_identical_identity_conversion_retires_the_collision() {
    let fixture = Fixture::new(MemoryScope::Project);
    let service = open_service(&fixture);
    let body = "这个项目使用 pnpm";
    let (structured, _, _) = applied(keyed(
        &service,
        &user("cmd-structured"),
        "keyed:1",
        "package_manager",
        body,
    ));
    let (explicit, revision, _) = applied(direct(&service, &user("cmd-1"), "direct:cmd-1", body));
    let outcome = attach_subject(
        &service,
        &user("cmd-2"),
        &explicit,
        revision,
        "package_manager",
    );
    assert!(
        matches!(outcome, WriteOutcome::Pending { .. }),
        "{outcome:?}"
    );
    let accepted = service
        .review(
            &user("reviewer"),
            ServiceScope::Project,
            &explicit,
            ReviewRequest::Accept,
        )
        .unwrap();
    assert!(
        matches!(accepted, WriteOutcome::Applied { .. }),
        "{accepted:?}"
    );
    let entries = active(&service);
    assert_eq!(entries.len(), 1, "one current fact after convergence");
    assert_eq!(entries[0].id, explicit);
    let archived = service
        .archived(
            &user("reader"),
            ServiceScope::Project,
            MemoryLanes::ALL,
            None,
        )
        .unwrap();
    assert!(
        archived.iter().any(|entry| entry.id == structured),
        "the identical structured record retires with history: {archived:?}"
    );
}

/// A credential typed into an update is refused exactly like one typed into a
/// direct write: the page must not be a second, weaker way to store a secret.
#[test]
fn a_secret_in_an_update_is_refused() {
    let fixture = Fixture::new(MemoryScope::Project);
    let service = open_service(&fixture);
    let (id, revision, _) = applied(direct(
        &service,
        &user("cmd-1"),
        "direct:cmd-1",
        "这个项目使用 pnpm",
    ));
    let refused = service.update(
        &user("cmd-2"),
        ServiceScope::Project,
        &id,
        revision,
        UpdateRequest {
            title: Some("API key".into()),
            body: Some("api_key=sk-abcdef1234567890abcdef".into()),
            value: None,
            tags: None,
            expires_at: None,
            subject: None,
        },
    );
    assert!(
        matches!(refused, Err(leveler_memory::ServiceError::Invalid(_))),
        "a secret-shaped update must be refused: {refused:?}"
    );
    let entries = active(&service);
    assert_eq!(entries.len(), 1);
    assert_eq!(
        entries[0].body, "这个项目使用 pnpm",
        "a refused update leaves the stored text alone"
    );
    assert_eq!(entries[0].revision, Some(revision));
    let mut leaked = Vec::new();
    collect_files(fixture.dir(), &mut leaked);
    for path in leaked {
        let bytes = std::fs::read(&path).unwrap_or_default();
        let text = String::from_utf8_lossy(&bytes);
        assert!(
            !text.contains("sk-abcdef1234567890abcdef"),
            "the refused secret must not survive on disk: {}",
            path.display()
        );
    }
}

/// A record whose title is blank cannot be found again by the person who wrote
/// it, so the service refuses it instead of writing one.
#[test]
fn an_update_cannot_blank_the_title() {
    let fixture = Fixture::new(MemoryScope::Project);
    let service = open_service(&fixture);
    let (id, revision, _) = applied(direct(
        &service,
        &user("cmd-1"),
        "direct:cmd-1",
        "这个项目使用 pnpm",
    ));
    let refused = service.update(
        &user("cmd-2"),
        ServiceScope::Project,
        &id,
        revision,
        UpdateRequest {
            title: Some("   ".into()),
            body: None,
            value: None,
            tags: None,
            expires_at: None,
            subject: None,
        },
    );
    assert!(
        matches!(refused, Err(leveler_memory::ServiceError::Invalid(_))),
        "a blank title must be refused: {refused:?}"
    );
    assert_eq!(active(&service)[0].revision, Some(revision));
}

/// A verified experience is verified *about the text the runtime observed*.
/// When a person rewrites that text, the runtime's proof no longer describes
/// it: the record keeps its history and its evidence references, but the
/// current revision may not keep claiming "verified".
#[test]
fn a_user_edit_of_a_verified_experience_drops_the_verification_claim() {
    let fixture = Fixture::new(MemoryScope::Project);
    let service = open_service(&fixture);
    let mut candidate = MemoryCandidate::new(
        "cargo build target",
        "cargo 构建失败时先清理 target 目录再重试",
        CandidateKind::Free,
        Some("build.cargo.target".to_string()),
        CandidateSource::RuntimeObserved,
        Vec::new(),
    )
    .unwrap();
    candidate.experience = Some(leveler_memory::ExperienceRecord {
        category: leveler_memory::ExperienceCategory::RootCause,
        verification: leveler_memory::ExperienceVerification::Verified,
        applies_when: "cargo build 在一个改了依赖的仓库里失败".to_string(),
        invalid_if: None,
        evidence: vec![
            leveler_memory::ExperienceEvidence {
                kind: "failure".into(),
                reference: "call-1 cargo build".into(),
            },
            leveler_memory::ExperienceEvidence {
                kind: "resolution".into(),
                reference: "call-2 cargo clean && cargo build".into(),
            },
        ],
    });
    let (id, revision, _) = applied(
        service
            .admit_candidate(&runtime(), ServiceScope::Project, &candidate, "inbox-1")
            .unwrap(),
    );
    let before = active(&service);
    let before = before.iter().find(|view| view.id == id).unwrap();
    assert_eq!(
        before.experience.as_ref().map(|e| e.standing.as_str()),
        Some("verified"),
        "the runtime's failure/resolution pair is what makes it verified"
    );

    let (updated, next, _) = applied(
        service
            .update(
                &user("cmd-edit"),
                ServiceScope::Project,
                &id,
                revision,
                UpdateRequest {
                    // The person rewrites the lesson to something the runtime
                    // never observed.
                    body: Some("依赖安装失败时改用 npm install".into()),
                    title: None,
                    value: None,
                    tags: None,
                    expires_at: None,
                    subject: None,
                },
            )
            .unwrap(),
    );
    assert_eq!(updated, id);
    assert_eq!(next, revision + 1);
    let after = active(&service);
    let after = after.iter().find(|view| view.id == id).unwrap();
    assert_eq!(
        after.body, "依赖安装失败时改用 npm install",
        "the user's text is what the record now says"
    );
    assert_eq!(
        after.experience.as_ref().map(|e| e.standing.as_str()),
        Some("observed"),
        "new text cannot inherit the runtime's verification of the old text"
    );
    assert_eq!(
        after.experience.as_ref().map(|e| e.evidence.len()),
        Some(2),
        "the runtime's evidence stays readable as audit of what was observed"
    );
}
