//! Stage 3: the logged injection receipt.
//!
//! `MemoryRecalled` is persisted and describes exactly what the builder put in
//! the request: ids, revisions, scopes, kinds, modes, sizes, the catalog the
//! prefix carried, and the delta. It never carries a body, and it never turns
//! a model claim into transport evidence.

mod memory_v2_harness;

use std::sync::Arc;

use leveler_agent::AgentEvent;
use leveler_memory::ServiceScope;
use memory_v2_harness::{
    ScriptedRuntime, assistant_text, executor, experience, fact, model_fact, receipt, run_turn,
};

#[test]
fn injection_receipt_shape_is_public() {
    let receipt = leveler_agent::MemoryInjectionReceipt {
        id: "mem_x".into(),
        revision: 1,
        scope: "project".into(),
        kind: "user_fact".into(),
        mode: "standing".into(),
        tokens: 12,
        title: "标题".into(),
        digest: "a".repeat(64),
    };
    let omission = leveler_agent::MemoryOmission {
        reason: "budget".into(),
        count: 1,
    };
    assert_eq!(receipt.mode, "standing");
    assert_eq!(omission.count, 1);
}

#[tokio::test]
async fn the_receipt_describes_what_the_request_actually_carries() {
    let fixture = memory_v2_harness::Fixture::new();
    let service = fixture.service();
    let id = fact(
        &service,
        ServiceScope::Project,
        "release_channel",
        "发布通道",
        "发布使用蓝色通道部署。",
    );
    let runtime = Arc::new(ScriptedRuntime::new(vec![assistant_text("蓝色")]));
    let executor = executor(fixture.workdir(), service.clone(), runtime.clone());
    let events = run_turn(&executor, "发布通道是什么").await;

    let AgentEvent::MemoryRecalled {
        count,
        ids,
        injections,
        omitted,
        budget,
        block_digest,
        catalog_ids,
        catalog_digest,
        catalog_version,
        catalog_delta_ids,
        catalog_delta_version,
        degraded,
        ..
    } = receipt(&events)
    else {
        unreachable!()
    };
    assert_eq!(*count, 1);
    assert_eq!(*ids, vec![id.clone()]);
    assert_eq!(injections.len(), 1);
    let injection = &injections[0];
    assert_eq!(injection.id, id);
    assert_eq!(injection.revision, 1);
    assert_eq!(injection.scope, "project");
    assert_eq!(injection.kind, "user_fact");
    assert_eq!(injection.mode, "standing");
    assert!(injection.tokens > 0);
    assert_eq!(injection.title, "发布通道");
    assert_eq!(injection.digest.len(), 64);
    // The standing line is also a lexical hit; it is removed from the
    // candidate set rather than paid for twice.
    assert!(
        omitted
            .iter()
            .all(|omission| omission.reason == "dedup" && omission.count == 1),
        "{omitted:?}"
    );
    assert_eq!(budget.total_tokens, 327);
    assert!(budget.used_tokens > 0 && budget.used_tokens <= budget.total_tokens);
    assert!(budget.standing_tokens > 0);
    assert_eq!(block_digest.as_deref().map(str::len), Some(64));
    assert_eq!(*catalog_ids, vec![id.clone()]);
    assert_eq!(catalog_digest.as_deref().map(str::len), Some(64));
    assert!(catalog_version.unwrap() >= 1);
    assert!(catalog_delta_ids.is_empty());
    assert!(catalog_delta_version.is_none());
    assert!(degraded.is_none());

    // Every receipt line is in the request the provider received; nothing is
    // claimed that was not sent.
    let trailing = runtime.requests()[0].control_context.trailing_text();
    assert!(
        trailing.contains(&format!("[{id} r1] 发布通道")),
        "{trailing}"
    );
    assert!(trailing.contains("发布使用蓝色通道部署。"));
    // The digest identifies the rendered line, not a body the log retained.
    let line = trailing
        .lines()
        .find(|line| line.contains(&id))
        .expect("receipt line");
    // The rendered line keeps its newline; the digest is over that exact text.
    assert_eq!(
        injection.digest,
        leveler_memory::v2::sha256_hex_text(&format!("{line}\n"))
    );
}

#[tokio::test]
async fn a_superseded_record_is_not_recalled_and_its_replacement_is() {
    let fixture = memory_v2_harness::Fixture::new();
    let service = fixture.service();
    let old = fact(
        &service,
        ServiceScope::Project,
        "billing_model",
        "计费模式",
        "计费使用 monthly 模式。",
    );
    let mut correction = leveler_memory::RememberRequest::new(
        memory_v2_harness::op(),
        ServiceScope::Project,
        "计费模式",
        "计费使用 annual 模式。",
        "preference",
    );
    correction.subject = Some("billing_model".into());
    correction.replaces_id = Some(old.clone());
    let new = service
        .remember(&memory_v2_harness::user(), correction)
        .unwrap()
        .applied_id()
        .expect("correction applies")
        .to_string();

    let runtime = Arc::new(ScriptedRuntime::new(vec![assistant_text("annual")]));
    let executor = executor(fixture.workdir(), service, runtime.clone());
    let events = run_turn(&executor, "计费模式是什么").await;
    let AgentEvent::MemoryRecalled { ids, .. } = receipt(&events) else {
        unreachable!()
    };
    assert!(!ids.contains(&old), "superseded record recalled: {ids:?}");
    assert!(ids.contains(&new), "replacement missing: {ids:?}");
    let trailing = runtime.requests()[0].control_context.trailing_text();
    assert!(!trailing.contains("monthly"), "{trailing}");
    assert!(trailing.contains("annual"), "{trailing}");
}

#[tokio::test]
async fn a_query_only_reaches_memories_it_is_lexically_related_to() {
    let fixture = memory_v2_harness::Fixture::new();
    let service = fixture.service();
    let deploy = model_fact(
        &service,
        ServiceScope::Project,
        "deploy_window",
        "部署窗口",
        "部署窗口是每周三凌晨。",
    );
    let runtime = Arc::new(ScriptedRuntime::new(vec![
        assistant_text("好的"),
        assistant_text("周三"),
    ]));
    let executor = executor(fixture.workdir(), service, runtime.clone());

    run_turn(&executor, "帮我改一个拼写错误").await;
    let unrelated = runtime.requests()[0].control_context.trailing_text();
    assert!(
        !unrelated.contains(&deploy),
        "an unrelated request preloaded a candidate: {unrelated}"
    );

    run_turn(&executor, "部署窗口在什么时候").await;
    let related = runtime.requests()[1].control_context.trailing_text();
    assert!(
        related.contains(&deploy),
        "a lexically related request did not retrieve the candidate: {related}"
    );
}

#[tokio::test]
async fn experience_is_recalled_in_its_own_bounded_mode() {
    let fixture = memory_v2_harness::Fixture::new();
    let service = fixture.service();
    let lesson = experience(
        &service,
        ServiceScope::Project,
        "构建失败",
        "cargo build 使用旧 target 目录时",
        "清理 target 后重新构建。",
    );
    let runtime = Arc::new(ScriptedRuntime::new(vec![assistant_text("清理 target")]));
    let executor = executor(fixture.workdir(), service, runtime.clone());
    let events = run_turn(&executor, "构建失败怎么办").await;

    let AgentEvent::MemoryRecalled {
        experience: experience_count,
        injections,
        budget,
        ..
    } = receipt(&events)
    else {
        unreachable!()
    };
    assert_eq!(*experience_count, 1);
    let lesson_receipt = injections
        .iter()
        .find(|injection| injection.id == lesson)
        .expect("experience receipt");
    assert_eq!(lesson_receipt.mode, "experience");
    assert!(budget.experience_tokens > 0);
    let trailing = runtime.requests()[0].control_context.trailing_text();
    assert!(trailing.contains("### Recalled experience"), "{trailing}");
    assert!(trailing.contains("applies when: cargo build"), "{trailing}");
    // The experience line carries applicability, not the body: the body is
    // one `memory.read` away and must not be paid for on every turn.
    assert!(!trailing.contains("清理 target 后重新构建。"), "{trailing}");
}

#[tokio::test]
async fn a_closed_experience_lane_withholds_experience_entirely() {
    let fixture = memory_v2_harness::Fixture::new();
    let service = fixture.service();
    experience(
        &service,
        ServiceScope::Project,
        "构建失败",
        "cargo build 使用旧 target 目录时",
        "清理 target 后重新构建。",
    );
    let runtime = Arc::new(ScriptedRuntime::new(vec![assistant_text("清理")]));
    let executor = executor(fixture.workdir(), service, runtime.clone())
        .with_memory_lanes(leveler_memory::MemoryLanes::USER_ONLY);
    let events = run_turn(&executor, "构建失败怎么办").await;
    let AgentEvent::MemoryRecalled {
        ids, experience, ..
    } = receipt(&events)
    else {
        unreachable!()
    };
    assert!(ids.is_empty(), "{ids:?}");
    assert_eq!(*experience, 0);
    let trailing = runtime.requests()[0].control_context.trailing_text();
    assert!(!trailing.contains("### Recalled experience"), "{trailing}");
    assert!(!trailing.contains("清理 target"), "{trailing}");
}

/// A lesson and a decision can share one subject — that is a correct data
/// model — but only the decision is advice for this turn. The lesson stays
/// stored and auditable; it is not injected as a parallel recommendation.
#[tokio::test]
async fn a_lesson_on_a_decided_subject_is_not_injected_beside_the_decision() {
    let fixture = memory_v2_harness::Fixture::new();
    let service = fixture.service();
    let key = memory_v2_harness::subject_key("package_manager");
    let decision = fact(
        &service,
        ServiceScope::Project,
        "package_manager",
        "包管理器",
        "本项目只使用 pnpm 安装依赖。",
    );
    let lesson = memory_v2_harness::experience_about(
        &service,
        ServiceScope::Project,
        &key,
        "依赖安装失败",
        "在没有 pnpm 缓存的机器上安装依赖时",
        "过去某个项目通过 npm install 解决了依赖问题。",
    );

    let runtime = Arc::new(ScriptedRuntime::new(vec![assistant_text("pnpm")]));
    let executor = executor(fixture.workdir(), service.clone(), runtime.clone());
    let events = run_turn(&executor, "怎么安装依赖").await;

    let AgentEvent::MemoryRecalled {
        ids,
        injections,
        omitted,
        experience: experience_count,
        ..
    } = receipt(&events)
    else {
        unreachable!()
    };
    assert!(
        ids.contains(&decision),
        "the user's decision is still injected"
    );
    assert!(
        injections.iter().all(|injection| injection.id != lesson),
        "the shadowed lesson was injected: {injections:?}"
    );
    assert_eq!(*experience_count, 0, "the experience section must be empty");
    assert!(
        omitted
            .iter()
            .any(|omission| omission.reason == "conflict" && omission.count == 1),
        "the omission has to be accounted for: {omitted:?}"
    );

    // The request itself carries the decision and not the lesson's conditions.
    let trailing = runtime.requests()[0].control_context.trailing_text();
    assert!(trailing.contains("本项目只使用 pnpm"), "{trailing}");
    assert!(
        !trailing.contains("npm install"),
        "a decided subject must not also carry the conflicting lesson: {trailing}"
    );
    assert!(!trailing.contains("### Recalled experience"), "{trailing}");

    // The lesson is still stored and still readable: the fix is about
    // injection, not about erasing history.
    let stored = service
        .listing_for_user(
            ServiceScope::Project,
            leveler_memory::MemoryLanes::EXPERIENCE_ONLY,
        )
        .unwrap();
    let stored = stored
        .iter()
        .find(|view| view.id == lesson)
        .expect("the shadowed lesson is still stored");
    assert_eq!(
        stored.body, "过去某个项目通过 npm install 解决了依赖问题。",
        "injection was withheld; the record was not deleted"
    );
}

/// The same shape, different subjects: nothing is dropped just because a
/// decision exists somewhere else in the store.
#[tokio::test]
async fn a_lesson_about_another_subject_is_still_recalled() {
    let fixture = memory_v2_harness::Fixture::new();
    let service = fixture.service();
    fact(
        &service,
        ServiceScope::Project,
        "package_manager",
        "包管理器",
        "本项目只使用 pnpm 安装依赖。",
    );
    let lesson = memory_v2_harness::experience_about(
        &service,
        ServiceScope::Project,
        &memory_v2_harness::subject_key("build_target"),
        "构建失败",
        "cargo build 使用旧 target 目录时",
        "清理 target 后重新构建。",
    );
    let runtime = Arc::new(ScriptedRuntime::new(vec![assistant_text("清理")]));
    let executor = executor(fixture.workdir(), service, runtime.clone());
    let events = run_turn(&executor, "构建失败怎么办").await;

    let AgentEvent::MemoryRecalled {
        injections,
        experience: experience_count,
        omitted,
        ..
    } = receipt(&events)
    else {
        unreachable!()
    };
    assert_eq!(*experience_count, 1);
    assert!(
        injections.iter().any(|injection| injection.id == lesson),
        "{injections:?}"
    );
    assert!(
        omitted.iter().all(|omission| omission.reason != "conflict"),
        "{omitted:?}"
    );
}
