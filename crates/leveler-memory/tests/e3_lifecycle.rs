//! E3 lifecycle acceptance: one source fact, one current memory.
//!
//! Every assertion reads the real store after the write path has run. The
//! model's reply, a decision enum alone, or a successful call is never enough:
//! the failure cases here were all "call succeeded, store kept both truths".

use leveler_memory::{
    CandidateRejection, CandidateSource, MemoryAuthority, MemoryCandidate, MemoryKind, MemoryStore,
    parse_direct_memory_command, parse_semantic_candidates, semantic_key_of, title_from_body,
    validate_semantic_candidate,
};

fn open() -> (tempfile::TempDir, MemoryStore) {
    let dir = tempfile::tempdir().unwrap();
    let store = MemoryStore::open(dir.path()).unwrap();
    (dir, store)
}

fn user_candidate(subject: &str, body: &str, value: Option<&str>) -> MemoryCandidate {
    let key = semantic_key_of(&leveler_memory::normalize_subject(subject));
    let mut candidate = MemoryCandidate::new(
        body,
        body,
        leveler_memory::CandidateKind::Free,
        Some(key.clone()),
        CandidateSource::UserExplicit,
        vec![],
    )
    .unwrap();
    candidate.semantic_key = Some(key);
    candidate.authority = MemoryAuthority::ExplicitUser;
    candidate.value = value.map(str::to_string);
    candidate
}

fn semantic_json(evidence: &str, extra: &str) -> String {
    format!(
        r#"[{{
            "fact": "billing_model 以按量计费为准",
            "subject": "billing_model",
            "value": "usage_based",
            "scope": "project",
            "durability": "durable",
            "memory_type": "project",
            "authority": "explicit_user",
            "operation_hint": "update",
            "evidence_span": "{evidence}",
            "confidence": 0.9
            {extra}
        }}]"#
    )
}

fn validated_candidate(raw: &str, message: &str) -> Option<MemoryCandidate> {
    let parsed = parse_semantic_candidates(raw).expect("fixture JSON must parse");
    assert_eq!(parsed.len(), 1, "fixture has one candidate");
    validate_semantic_candidate(&parsed[0], message).ok()
}

fn active_user_bodies(store: &MemoryStore) -> Vec<String> {
    store
        .effective_active()
        .unwrap()
        .into_iter()
        .map(|entry| entry.body)
        .collect()
}

/// U1: the direct command writes a keyless entry and the same turn's
/// extraction writes a keyed one in different words. Both writers describe one
/// user fact, so exactly one current memory may remain.
#[test]
fn u1_one_direct_fact_written_by_two_entries_has_one_active() {
    let (_dir, store) = open();
    let command = "请记住：billing_model 订阅制";
    let body = parse_direct_memory_command(command).expect("direct command must parse");
    let direct = store
        .activate(
            &title_from_body(&body),
            &body,
            MemoryKind::Preference,
            vec![],
        )
        .unwrap();
    assert_eq!(store.effective_active().unwrap().len(), 1);

    let raw = r#"[{
        "fact": "billing_model 使用订阅制计费",
        "subject": "billing_model",
        "value": "subscription",
        "scope": "project",
        "durability": "durable",
        "memory_type": "project",
        "authority": "explicit_user",
        "operation_hint": "create",
        "evidence_span": "请记住：billing_model 订阅制",
        "confidence": 0.9
    }]"#;
    let parsed = parse_semantic_candidates(raw).expect("fixture JSON must parse");
    let result = validate_semantic_candidate(&parsed[0], command);
    assert!(
        matches!(result, Err(CandidateRejection::DirectCommandOwned)),
        "a direct command turn is owned by the direct writer: {result:?}"
    );

    let active = store.effective_active().unwrap();
    assert_eq!(
        active.len(),
        1,
        "one user fact must not stay current twice: {:?}",
        active
            .iter()
            .map(|entry| (&entry.id, &entry.body))
            .collect::<Vec<_>>()
    );
    assert_eq!(active[0].id, direct.id, "the direct writer is the owner");
    assert!(
        store.list_archived().unwrap().is_empty(),
        "rejecting a second writer is not history"
    );
}

/// U2: two writers restate one source fact under different subjects but with
/// the exact same body. The body is the structured statement, so the lifecycle
/// must not accumulate a second Active entry for it.
#[test]
fn u2_the_same_body_under_a_different_subject_is_one_memory() {
    let (_dir, store) = open();
    let first = user_candidate(
        "billing_setting_invoice_header",
        "发票抬头使用 ACME 有限公司",
        Some("ACME 有限公司"),
    );
    store.commit_candidate(&first).unwrap();

    let second = user_candidate(
        "billing_setting_invoice_title",
        "发票抬头使用 ACME 有限公司",
        Some("ACME 有限公司"),
    );
    let outcome = store.commit_candidate(&second).unwrap();
    let _ = outcome;
    assert_eq!(
        store.effective_active().unwrap().len(),
        1,
        "the same source fact must not stay current under two subjects"
    );
}

/// U2: a later writer that reuses the existing subject converges without a
/// second current entry. This is the contract the extractor context must
/// preserve.
#[test]
fn u2_a_reused_subject_keeps_one_current_value() {
    let (_dir, store) = open();
    store
        .commit_candidate(&user_candidate(
            "billing_setting_invoice_header",
            "发票抬头使用 ACME 有限公司",
            Some("ACME 有限公司"),
        ))
        .unwrap();
    store
        .commit_candidate(&user_candidate(
            "billing_setting_invoice_header",
            "发票抬头仍使用 ACME 有限公司",
            Some("ACME 有限公司"),
        ))
        .unwrap();
    assert_eq!(store.effective_active().unwrap().len(), 1);
}

/// U3: a correction names the exact keyless memory it replaces. The old fact
/// has no key, but the target id is structural evidence; it must leave current
/// truth, not survive beside its replacement.
#[test]
fn u3_a_correction_naming_a_keyless_memory_removes_it_from_current_truth() {
    let (_dir, store) = open();
    let body = parse_direct_memory_command("请记住：billing_model 订阅制").unwrap();
    let old = store
        .activate(&title_from_body(&body), &body, MemoryKind::Preference, vec![])
        .unwrap();

    let message = "billing_model 改成按量计费，订阅制不再使用";
    let raw = format!(
        r#"[{{
            "fact": "billing_model 改为按量计费",
            "subject": "billing_model",
            "value": "usage_based",
            "scope": "project",
            "durability": "durable",
            "memory_type": "project",
            "authority": "explicit_user",
            "operation_hint": "update",
            "evidence_span": "billing_model 改成按量计费",
            "confidence": 0.95,
            "statement": "correction",
            "replaces_memory_id": "{}"
        }}]"#,
        old.id
    );
    let candidate = validated_candidate(&raw, message).expect("correction must validate");
    store.admit_candidate(candidate).unwrap();

    let active = store.effective_active().unwrap();
    assert_eq!(active.len(), 1, "old keyless truth must not stay current");
    assert!(active[0].body.contains("按量计费"));
    let archived = store.list_archived().unwrap();
    assert!(
        archived
            .iter()
            .any(|entry| entry.id == old.id && entry.status == leveler_memory::MemoryStatus::Superseded),
        "the replaced direct entry must be historical, not deleted"
    );
}

/// U5: a new session reads only the current decision. The replaced keyless
/// value must not be injected beside the correction.
#[test]
fn u5_a_new_session_sees_only_the_current_billing_model() {
    let (dir, store) = open();
    let body = parse_direct_memory_command("请记住：billing_model 订阅制").unwrap();
    let old = store
        .activate(&title_from_body(&body), &body, MemoryKind::Preference, vec![])
        .unwrap();
    let message = "billing_model 改成按量计费";
    let raw = format!(
        r#"[{{
            "fact": "billing_model 改为按量计费",
            "subject": "billing_model",
            "value": "usage_based",
            "scope": "project",
            "durability": "durable",
            "memory_type": "project",
            "authority": "explicit_user",
            "operation_hint": "update",
            "evidence_span": "billing_model 改成按量计费",
            "confidence": 0.95,
            "statement": "correction",
            "replaces_memory_id": "{}"
        }}]"#,
        old.id
    );
    if let Some(candidate) = validated_candidate(&raw, message) {
        store.admit_candidate(candidate).unwrap();
    }
    drop(store);

    let reopened = MemoryStore::open(dir.path()).unwrap();
    let bodies = active_user_bodies(&reopened);
    assert_eq!(bodies.len(), 1, "new session must see one truth: {bodies:?}");
    assert!(bodies[0].contains("按量计费"));
    let hits = reopened.search("billing_model", 10).unwrap();
    assert!(
        !hits
            .iter()
            .any(|(entry, _)| entry.body.contains("订阅制")),
        "the replaced value must not be recallable as current"
    );
}

/// U6a: the model labels a discussed option `explicit_user`. That label cannot
/// turn a proposal into the user's decision; the formal decision stays current
/// and no second record is written.
#[test]
fn u6a_a_proposal_cannot_replace_a_formal_decision() {
    let (_dir, store) = open();
    store
        .commit_candidate(&user_candidate(
            "billing_model",
            "billing_model 使用订阅制",
            Some("subscription"),
        ))
        .unwrap();
    let message = "我建议按量计费更合适，只是评估建议，不是正式决定。";
    let raw = semantic_json(
        "只是评估建议，不是正式决定。",
        r#", "statement": "proposal", "related_subject": "billing_model""#,
    );
    if let Some(candidate) = validated_candidate(&raw, message) {
        store.admit_candidate(candidate).unwrap();
    }

    let active = store.effective_active().unwrap();
    assert_eq!(active.len(), 1, "a proposal is not a second decision");
    assert_eq!(active[0].value.as_deref(), Some("subscription"));
    assert_eq!(
        store.list_archived().unwrap().len(),
        0,
        "a proposal must not move the formal decision to history"
    );
}

/// U6a: a hypothetical option is not a decision either.
#[test]
fn u6a_a_hypothetical_cannot_replace_a_formal_decision() {
    let (_dir, store) = open();
    store
        .commit_candidate(&user_candidate(
            "billing_model",
            "billing_model 使用订阅制",
            Some("subscription"),
        ))
        .unwrap();
    let message = "如果以后改成按量计费呢？只是假设。";
    let raw = semantic_json(
        "如果以后改成按量计费呢？只是假设。",
        r#", "statement": "hypothetical", "related_subject": "billing_model""#,
    );
    if let Some(candidate) = validated_candidate(&raw, message) {
        store.admit_candidate(candidate).unwrap();
    }
    assert_eq!(store.effective_active().unwrap().len(), 1);
    assert_eq!(
        store.effective_active().unwrap()[0].value.as_deref(),
        Some("subscription")
    );
}

/// The counter-case: a confirmed correction from the user does replace the
/// formal decision. The statement contract must not block ordinary updates.
#[test]
fn u6a_a_confirmed_correction_does_replace_the_formal_decision() {
    let (_dir, store) = open();
    store
        .commit_candidate(&user_candidate(
            "billing_model",
            "billing_model 使用订阅制",
            Some("subscription"),
        ))
        .unwrap();
    let message = "billing_model 改成按量计费。";
    let raw = semantic_json(
        "billing_model 改成按量计费。",
        r#", "statement": "correction", "related_subject": "billing_model""#,
    );
    let candidate = validated_candidate(&raw, message).expect("correction must validate");
    store.admit_candidate(candidate).unwrap();

    let active = store.effective_active().unwrap();
    assert_eq!(active.len(), 1);
    assert_eq!(active[0].value.as_deref(), Some("usage_based"));
    assert_eq!(store.list_archived().unwrap().len(), 1);
}

/// Pending cleanup is exact: a pending candidate that states the same fact as
/// the committed one is cleared, and the removal is traceable to the commit.
#[test]
fn a_covered_pending_is_cleared_with_a_traceable_disposition() {
    let (dir, store) = open();
    let pending = user_candidate("billing_model", "billing_model 使用订阅制", Some("subscription"));
    store.propose(pending.clone()).unwrap();
    assert_eq!(store.list_pending().unwrap().len(), 1);

    let committed = user_candidate("billing_model", "billing_model 使用订阅制", Some("subscription"));
    store.commit_candidate(&committed).unwrap();

    assert!(
        store.list_pending().unwrap().is_empty(),
        "the proposal is now covered by current truth"
    );
    let history = std::fs::read_to_string(dir.path().join("pending_history.jsonl"))
        .expect("every pending settlement must leave a disposition record");
    assert!(
        history.contains(&pending.id),
        "disposition must name the settled candidate: {history}"
    );
    assert!(
        history.contains("covered"),
        "disposition must say why it was cleared: {history}"
    );
}

/// Pending cleanup is not "same subject": a different value is an alternative
/// the user has not decided, so an unrelated commit must not erase it.
#[test]
fn a_pending_with_a_different_value_survives_a_commit_of_the_same_subject() {
    let (_dir, store) = open();
    let alternative = user_candidate(
        "billing_model",
        "billing_model 也许改为按量计费",
        Some("usage_based"),
    );
    store.propose(alternative).unwrap();

    store
        .commit_candidate(&user_candidate(
            "billing_model",
            "billing_model 使用订阅制",
            Some("subscription"),
        ))
        .unwrap();

    let pending = store.list_pending().unwrap();
    assert_eq!(
        pending.len(),
        1,
        "a different, unconfirmed value must stay available for the user"
    );
    assert_eq!(pending[0].value.as_deref(), Some("usage_based"));
}

/// Guard against the fix widening into "drop every candidate": a plain new
/// fact still becomes current.
#[test]
fn a_new_confirmed_fact_still_becomes_current() {
    let (_dir, store) = open();
    let fact = user_candidate(
        "release_probe_codename",
        "发布探针代号固定为 ORANGE-7319",
        Some("ORANGE-7319"),
    );
    store.admit_candidate(fact).unwrap();
    assert_eq!(store.effective_active().unwrap().len(), 1);
}

